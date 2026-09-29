//! Copilot permission notifications correlated with the SDK's async permission handler.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use github_copilot_sdk::{
    PermissionDecision, PermissionDecisionApproveOnce, PermissionDecisionReject,
    PermissionRequestData, PermissionRequestKind, RequestId, SessionId,
    handler::{PermissionHandler, PermissionResult},
    rpc::{
        PermissionDecisionApproveForSession, PermissionDecisionApproveForSessionApproval,
        PermissionDecisionApproveForSessionApprovalCommands,
        PermissionDecisionApproveForSessionApprovalCustomTool,
        PermissionDecisionApproveForSessionApprovalMcp,
        PermissionDecisionApproveForSessionApprovalRead,
        PermissionDecisionApproveForSessionApprovalWrite, PermissionDecisionRequest,
    },
    session::Session as NativeSession,
};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, mpsc};

use crate::{
    broker::BROKER_SERVER_NAME,
    protocol::{
        Approval, ApprovalId, ApprovalSubject, CommandAction, CopilotPermissions, Decision,
    },
    provider::{AttributedProviderEvent, ProviderError, ProviderEvent, ProviderEventAttribution},
};

use super::super::command_presentation::{PresentedCommand, present_command_for_approval};
use super::{copilot_error, tools::tool_activity_id};

#[derive(Clone)]
pub(super) struct CopilotApprovals {
    shared: Arc<Shared>,
}

struct Shared {
    events: mpsc::UnboundedSender<Result<AttributedProviderEvent, ProviderError>>,
    pending: Mutex<Pending>,
    settlements: Arc<AsyncMutex<()>>,
    permissions: Mutex<CopilotPermissions>,
    execution_directory: PathBuf,
}

#[derive(Default)]
struct Pending {
    requests: HashMap<RequestId, PermissionRequestData>,
    observations: HashMap<RequestId, ProviderEventAttribution>,
    approvals: HashMap<ApprovalId, NativeApproval>,
}

struct NativeApproval {
    request_id: RequestId,
    data: PermissionRequestData,
    attribution: ProviderEventAttribution,
}

pub(super) struct NativeDecisionDelivery {
    pub(super) settlement: OwnedMutexGuard<()>,
}

impl CopilotApprovals {
    pub(super) fn new(
        events: mpsc::UnboundedSender<Result<AttributedProviderEvent, ProviderError>>,
        permissions: CopilotPermissions,
        execution_directory: PathBuf,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                events,
                pending: Mutex::new(Pending::default()),
                settlements: Arc::new(AsyncMutex::new(())),
                permissions: Mutex::new(permissions),
                execution_directory,
            }),
        }
    }

    pub(super) fn observe(&self, request_id: RequestId, attribution: ProviderEventAttribution) {
        let event = {
            let mut pending = self
                .shared
                .pending
                .lock()
                .expect("Copilot Approval lock is not poisoned");
            pending.observations.insert(request_id.clone(), attribution);
            pair_request(&mut pending, &request_id, &self.shared.execution_directory)
        };
        if let Some(event) = event {
            let _ = self.shared.events.send(Ok(event));
        }
    }

    pub(super) fn complete(&self, request_id: &RequestId) -> Option<AttributedProviderEvent> {
        let mut pending = self
            .shared
            .pending
            .lock()
            .expect("Copilot Approval lock is not poisoned");
        pending.requests.remove(request_id);
        pending.observations.remove(request_id);
        let id = pending
            .approvals
            .iter()
            .find_map(|(id, native)| (&native.request_id == request_id).then_some(*id))?;
        let attribution = pending
            .approvals
            .remove(&id)
            .expect("matched Copilot Approval exists")
            .attribution;
        Some(AttributedProviderEvent {
            attribution,
            event: ProviderEvent::ApprovalWithdrawn { id },
        })
    }

    pub(super) fn clear(&self) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .expect("Copilot Approval lock is not poisoned");
        *pending = Pending::default();
    }

    pub(super) fn adopt_posture(&self, permissions: CopilotPermissions) {
        *self
            .shared
            .permissions
            .lock()
            .expect("Copilot Approval Posture lock is not poisoned") = permissions;
    }

    pub(super) fn settle(&self, attribution: &ProviderEventAttribution) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .expect("Copilot Approval lock is not poisoned");
        let settled_requests = pending
            .observations
            .iter()
            .filter_map(|(request, owner)| (owner == attribution).then_some(request.clone()))
            .collect::<Vec<_>>();
        for request in settled_requests {
            pending.observations.remove(&request);
            pending.requests.remove(&request);
        }
        pending
            .approvals
            .retain(|_, approval| &approval.attribution != attribution);
    }

    pub(super) async fn wait_for_settled_decision(&self) {
        drop(self.shared.settlements.lock().await);
    }

    pub(super) async fn submit(
        &self,
        native: &NativeSession,
        id: ApprovalId,
        decision: Decision,
    ) -> Result<NativeDecisionDelivery, ProviderError> {
        let approval = self
            .shared
            .pending
            .lock()
            .expect("Copilot Approval lock is not poisoned")
            .approvals
            .remove(&id)
            .ok_or_else(|| ProviderError::decision_rejected("Copilot Approval is unavailable"))?;
        let result = native_decision(decision, &approval.data);
        let settlement = self.shared.settlements.clone().lock_owned().await;
        let applied = native
            .rpc()
            .permissions()
            .handle_pending_permission_request(PermissionDecisionRequest {
                decision_context: None,
                request_id: approval.request_id,
                result,
            })
            .await
            .map_err(|error| copilot_error(format!("Copilot Approval delivery failed: {error}")))?;
        if !applied.success {
            return Err(ProviderError::decision_withdrawn(
                "Copilot Approval was already resolved",
            ));
        }
        Ok(NativeDecisionDelivery { settlement })
    }
}

#[async_trait::async_trait]
impl PermissionHandler for CopilotApprovals {
    async fn handle(
        &self,
        _session_id: SessionId,
        request_id: RequestId,
        data: PermissionRequestData,
    ) -> PermissionResult {
        // A Broker Tool never asks an Approval (ADR 0035), whatever the Session's posture: its
        // calls are Suru's own to permit, and Copilot asks this handler before every one of them,
        // a native Subagent's included (docs/validation/0408-copilot-mcp-tool-timeout.md). Only a
        // managed policy demanding an explicit human decision still takes the path it always has.
        if is_broker_call(&data) && data.managed_approval_required != Some(true) {
            return PermissionResult::approve_once();
        }
        let managed_settings_enabled = data.managed_settings_enabled
            || request(&data)["managedSettingsEnabled"]
                .as_bool()
                .unwrap_or(false);
        let permissions = *self
            .shared
            .permissions
            .lock()
            .expect("Copilot Approval Posture lock is not poisoned");
        let managed_user_decision =
            permissions == CopilotPermissions::AllowAll && managed_settings_enabled;
        if !managed_user_decision && data.managed_approval_required == Some(true) {
            return PermissionResult::no_result();
        }
        if permissions == CopilotPermissions::AllowAll && !managed_user_decision {
            return PermissionResult::approve_once();
        }
        let event = {
            let mut pending = self
                .shared
                .pending
                .lock()
                .expect("Copilot Approval lock is not poisoned");
            pending.requests.insert(request_id.clone(), data);
            pair_request(&mut pending, &request_id, &self.shared.execution_directory)
        };
        if let Some(event) = event {
            let _ = self.shared.events.send(Ok(event));
        }
        PermissionResult::no_result()
    }
}

fn pair_request(
    pending: &mut Pending,
    request_id: &RequestId,
    execution_directory: &Path,
) -> Option<AttributedProviderEvent> {
    if !pending.requests.contains_key(request_id) || !pending.observations.contains_key(request_id)
    {
        return None;
    }
    let data = pending
        .requests
        .remove(request_id)
        .expect("checked request exists");
    let attribution = pending
        .observations
        .remove(request_id)
        .expect("checked observation exists");
    let approval = Approval {
        id: ApprovalId::new(),
        subject: subject(&data, execution_directory),
        reason: reason(&data),
    };
    // Every tool execution's row is named by its tool call alone, so the Approval links to
    // whichever row the call became — a Command, or a Tool Call — without guessing which.
    let tool_activity_id = data.tool_call_id.as_deref().map(tool_activity_id);
    pending.approvals.insert(
        approval.id,
        NativeApproval {
            request_id: request_id.clone(),
            data,
            attribution: attribution.clone(),
        },
    );
    Some(AttributedProviderEvent {
        attribution,
        event: ProviderEvent::ApprovalRequested {
            approval,
            tool_activity_id,
        },
    })
}

fn request(data: &PermissionRequestData) -> &Value {
    data.extra.get("permissionRequest").unwrap_or(&data.extra)
}

/// Whether `data` asks to call one of the Broker's Tools: an MCP call to the server Suru handed
/// every Session the Broker as.
fn is_broker_call(data: &PermissionRequestData) -> bool {
    data.kind == Some(PermissionRequestKind::Mcp)
        && request(data)["serverName"] == BROKER_SERVER_NAME
}

fn reason(data: &PermissionRequestData) -> Option<String> {
    let request = request(data);
    request
        .get("intention")
        .or_else(|| request.get("warning"))
        .or_else(|| request.get("requestSandboxBypassReason"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn subject(data: &PermissionRequestData, execution_directory: &Path) -> ApprovalSubject {
    let value = request(data);
    match data.kind {
        Some(PermissionRequestKind::Shell) => {
            let PresentedCommand { command, cwd } = present_command_for_approval(
                value["fullCommandText"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            );
            ApprovalSubject::Command {
                command,
                cwd: cwd.or_else(|| Some(execution_directory.to_owned())),
                actions: value["commands"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|command| command["identifier"].as_str())
                    .map(|command| CommandAction::Unknown {
                        command: command.to_owned(),
                    })
                    .collect(),
            }
        }
        Some(PermissionRequestKind::Write) => ApprovalSubject::FileChange {
            paths: value["fileName"]
                .as_str()
                .map(PathBuf::from)
                .into_iter()
                .collect(),
            grant_root: None,
        },
        Some(PermissionRequestKind::Read) => ApprovalSubject::Read {
            path: value["path"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_default(),
        },
        Some(PermissionRequestKind::Url) => ApprovalSubject::Network {
            host_or_url: value["url"].as_str().unwrap_or_default().to_owned(),
        },
        Some(PermissionRequestKind::Mcp) => ApprovalSubject::OtherTool {
            name: format!(
                "MCP {}/{}",
                value["serverName"].as_str().unwrap_or_default(),
                value["toolName"].as_str().unwrap_or_default()
            ),
            input: value.get("args").cloned().unwrap_or(Value::Null),
        },
        Some(PermissionRequestKind::CustomTool) => ApprovalSubject::OtherTool {
            name: value["toolName"]
                .as_str()
                .or_else(|| value["name"].as_str())
                .unwrap_or("custom tool")
                .to_owned(),
            input: value.get("input").cloned().unwrap_or_else(|| value.clone()),
        },
        _ => ApprovalSubject::OtherTool {
            name: value["kind"].as_str().unwrap_or("unknown").to_owned(),
            input: value.clone(),
        },
    }
}

fn native_decision(decision: Decision, data: &PermissionRequestData) -> PermissionDecision {
    match decision {
        Decision::Accept => {
            PermissionDecision::ApproveOnce(PermissionDecisionApproveOnce::default())
        }
        Decision::Decline | Decision::DeclineAndInterrupt => {
            PermissionDecision::Reject(PermissionDecisionReject::default())
        }
        Decision::AcceptForSession => session_decision(data),
    }
}

fn session_decision(data: &PermissionRequestData) -> PermissionDecision {
    let value = request(data);
    let approval = match data.kind {
        Some(PermissionRequestKind::Shell) => {
            Some(PermissionDecisionApproveForSessionApproval::Commands(
                PermissionDecisionApproveForSessionApprovalCommands {
                    command_identifiers: value["commands"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|command| command["identifier"].as_str().map(str::to_owned))
                        .collect(),
                    ..Default::default()
                },
            ))
        }
        Some(PermissionRequestKind::Read) => {
            Some(PermissionDecisionApproveForSessionApproval::Read(
                PermissionDecisionApproveForSessionApprovalRead::default(),
            ))
        }
        Some(PermissionRequestKind::Write) => {
            Some(PermissionDecisionApproveForSessionApproval::Write(
                PermissionDecisionApproveForSessionApprovalWrite::default(),
            ))
        }
        Some(PermissionRequestKind::Mcp) => Some(PermissionDecisionApproveForSessionApproval::Mcp(
            PermissionDecisionApproveForSessionApprovalMcp {
                server_name: value["serverName"].as_str().unwrap_or_default().to_owned(),
                tool_name: value["toolName"].as_str().map(str::to_owned),
                ..Default::default()
            },
        )),
        Some(PermissionRequestKind::CustomTool) => {
            Some(PermissionDecisionApproveForSessionApproval::CustomTool(
                PermissionDecisionApproveForSessionApprovalCustomTool {
                    tool_name: value["toolName"]
                        .as_str()
                        .or_else(|| value["name"].as_str())
                        .unwrap_or("custom tool")
                        .to_owned(),
                    ..Default::default()
                },
            ))
        }
        _ => None,
    };
    let domain = (data.kind == Some(PermissionRequestKind::Url))
        .then(|| value["url"].as_str().map(url_domain))
        .flatten();
    PermissionDecision::ApproveForSession(PermissionDecisionApproveForSession {
        approval,
        domain,
        ..Default::default()
    })
}

fn url_domain(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}
