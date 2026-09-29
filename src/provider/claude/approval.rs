//! Correlated Claude `can_use_tool` permission callbacks. Each Approval links to the row recording
//! the use it gates — its Command, File Change, or Tool Call — and a Decision declining the use
//! settles that row as failed.

use super::super::command_presentation::{PresentedCommand, present_command_for_approval};
use super::{
    claude_error,
    projection::gated_tool_activity_id,
    transport::{ConversationItem, ConversationSink, StreamJsonTransport},
};
use crate::{
    protocol::{Approval, ApprovalId, ApprovalSubject, Decision},
    provider::{
        AttributedProviderEvent, ProviderDecisionDelivery, ProviderError, ProviderEvent,
        ProviderEventAttribution,
    },
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

const DECLINED_MESSAGE: &str = "User declined the tool request";

#[derive(Default)]
pub(super) struct ClaudeApprovals {
    pending: Mutex<HashMap<ApprovalId, NativeApproval>>,
    transport: Mutex<Option<StreamJsonTransport>>,
}

struct NativeApproval {
    attribution: ProviderEventAttribution,
    request_id: String,
    tool_name: String,
    /// The tool use the Approval gates, where the request names it.
    tool_use_id: Option<String>,
    input: Value,
    permission_suggestions: Vec<Value>,
}

impl ClaudeApprovals {
    pub(super) fn connect(&self, transport: StreamJsonTransport) {
        self.clear();
        *self
            .transport
            .lock()
            .expect("Claude Approval transport lock is not poisoned") = Some(transport);
    }

    pub(super) fn clear(&self) {
        self.pending
            .lock()
            .expect("Claude Approval lock is not poisoned")
            .clear();
    }

    pub(super) fn settle(&self, attribution: &ProviderEventAttribution) {
        self.pending
            .lock()
            .expect("Claude Approval lock is not poisoned")
            .retain(|_, approval| &approval.attribution != attribution);
    }

    pub(super) async fn receive(
        &self,
        message: &Value,
        attribution: Option<ProviderEventAttribution>,
        execution_directory: &std::path::Path,
    ) -> Result<Option<Vec<AttributedProviderEvent>>, ProviderError> {
        match message.get("type").and_then(Value::as_str) {
            Some("control_cancel_request") => {
                let Some(request_id) = message.get("request_id").and_then(Value::as_str) else {
                    return Ok(None);
                };
                let mut pending = self
                    .pending
                    .lock()
                    .expect("Claude Approval lock is not poisoned");
                let Some(id) = pending
                    .iter()
                    .find_map(|(id, approval)| (approval.request_id == request_id).then_some(*id))
                else {
                    return Ok(None);
                };
                let native = pending
                    .remove(&id)
                    .expect("pending Approval correlation exists");
                Ok(Some(vec![AttributedProviderEvent {
                    attribution: native.attribution,
                    event: ProviderEvent::ApprovalWithdrawn { id },
                }]))
            }
            Some("control_request") if message["request"]["subtype"] == "can_use_tool" => {
                let request_id = message
                    .get("request_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        claude_error("Claude Approval control request has no identity")
                    })?;
                let request = &message["request"];
                let Some(tool_name) = request["tool_name"].as_str() else {
                    return Err(claude_error("Claude Approval request has no Tool name"));
                };
                if tool_name == "AskUserQuestion" {
                    return Ok(None);
                }
                let Some(attribution) = attribution else {
                    let transport = self.transport()?;
                    Self::send_response(
                        &transport,
                        request_id,
                        json!({"behavior":"deny", "message":"The owning Subagent could not be identified", "interrupt":false}),
                    )
                    .await?;
                    return Ok(Some(vec![]));
                };
                let input = request["input"].clone();
                let tool_use_id = request["tool_use_id"].as_str().map(str::to_owned);
                let approval = Approval {
                    id: ApprovalId::new(),
                    subject: subject(tool_name, &input, execution_directory),
                    reason: request["decision_reason"]
                        .as_str()
                        .or_else(|| request["reason"].as_str())
                        .map(str::to_owned),
                };
                let mut pending = self
                    .pending
                    .lock()
                    .expect("Claude Approval lock is not poisoned");
                if pending
                    .values()
                    .any(|approval| approval.request_id == request_id)
                {
                    return Err(claude_error(
                        "Claude reused a live Approval callback identity",
                    ));
                }
                pending.insert(
                    approval.id,
                    NativeApproval {
                        attribution: attribution.clone(),
                        request_id: request_id.to_owned(),
                        tool_name: tool_name.to_owned(),
                        tool_use_id: tool_use_id.clone(),
                        input,
                        permission_suggestions: request["permission_suggestions"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default(),
                    },
                );
                let tool_activity_id = tool_use_id.as_deref().and_then(|tool_use_id| {
                    gated_tool_activity_id(tool_name, tool_use_id, &request["input"])
                });
                Ok(Some(vec![AttributedProviderEvent {
                    attribution,
                    event: ProviderEvent::ApprovalRequested {
                        approval,
                        tool_activity_id,
                    },
                }]))
            }
            _ => Ok(None),
        }
    }

    /// Delivers `decision` to the CLI. A declined use will never run, so once the CLI is told, the
    /// Session's `conversation` hears of it too, and the row recording the use settles as failed.
    pub(super) async fn submit(
        &self,
        id: ApprovalId,
        decision: Decision,
        conversation: &ConversationSink,
    ) -> Result<ProviderDecisionDelivery, ProviderError> {
        let native = self
            .pending
            .lock()
            .expect("Claude Approval lock is not poisoned")
            .remove(&id)
            .ok_or_else(|| ProviderError::decision_rejected("Claude Approval is unavailable"))?;
        let response = match decision {
            Decision::Accept => json!({"behavior":"allow", "updatedInput":native.input}),
            Decision::AcceptForSession => {
                let mut updates = native.permission_suggestions;
                for update in &mut updates {
                    if let Some(object) = update.as_object_mut() {
                        object.insert("destination".into(), Value::String("session".into()));
                    }
                }
                if updates.is_empty() {
                    updates.push(json!({
                        "type":"addRules",
                        "rules":[{"toolName":native.tool_name}],
                        "behavior":"allow",
                        "destination":"session"
                    }));
                }
                json!({"behavior":"allow", "updatedInput":native.input, "updatedPermissions":updates})
            }
            Decision::Decline => {
                json!({"behavior":"deny", "message":DECLINED_MESSAGE, "interrupt":false})
            }
            Decision::DeclineAndInterrupt => {
                json!({"behavior":"deny", "message":DECLINED_MESSAGE, "interrupt":true})
            }
        };
        let declined = matches!(decision, Decision::Decline | Decision::DeclineAndInterrupt);
        let transport = self.transport()?;
        let settlement = transport.decision_settlement()?;
        Self::send_response(&transport, &native.request_id, response).await?;
        if declined && let Some(tool_use_id) = native.tool_use_id {
            let _ = conversation.send(Ok(ConversationItem::ToolUseDeclined {
                tool_use_id,
                input: native.input,
                message: DECLINED_MESSAGE.to_owned(),
            }));
        }
        Ok(ProviderDecisionDelivery::with_follow_up(Box::pin(
            async move {
                drop(settlement);
                Ok(())
            },
        )))
    }

    fn transport(&self) -> Result<StreamJsonTransport, ProviderError> {
        self.transport
            .lock()
            .expect("Claude Approval transport lock is not poisoned")
            .clone()
            .ok_or_else(|| claude_error("Claude Approval transport is unavailable"))
    }

    async fn send_response(
        transport: &StreamJsonTransport,
        request_id: &str,
        response: Value,
    ) -> Result<(), ProviderError> {
        transport
            .send(&json!({"type":"control_response","response":{"subtype":"success","request_id":request_id,"response":response}}))
            .await
    }
}

fn subject(
    tool_name: &str,
    input: &Value,
    execution_directory: &std::path::Path,
) -> ApprovalSubject {
    let path = |key: &str| input[key].as_str().map(PathBuf::from);
    match tool_name {
        "Bash" => {
            let PresentedCommand { command, cwd } = present_command_for_approval(
                input["command"].as_str().unwrap_or_default().to_owned(),
            );
            ApprovalSubject::Command {
                command,
                cwd: cwd
                    .or_else(|| path("cwd"))
                    .or_else(|| Some(execution_directory.to_owned())),
                actions: Vec::new(),
            }
        }
        "Edit" | "MultiEdit" | "Write" => ApprovalSubject::FileChange {
            paths: path("file_path").into_iter().collect(),
            grant_root: None,
        },
        "NotebookEdit" => ApprovalSubject::FileChange {
            paths: path("notebook_path").into_iter().collect(),
            grant_root: None,
        },
        "Read" => ApprovalSubject::Read {
            path: path("file_path").unwrap_or_default(),
        },
        "WebFetch" => ApprovalSubject::Network {
            host_or_url: input["url"].as_str().unwrap_or_default().to_owned(),
        },
        _ => ApprovalSubject::OtherTool {
            name: tool_name.to_owned(),
            input: input.clone(),
        },
    }
}
