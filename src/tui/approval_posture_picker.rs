//! The small fixed picker for a Session's Provider-native Approval Posture.

use crate::protocol::{
    ApprovalPosture, ClaudePermissionMode, CodexApprovalPolicy, CodexSandboxMode,
    CopilotPermissions, UpdateApprovalPostureRequest,
};

use super::list_window::ListWindow;

#[derive(Clone, Copy, Debug)]
pub(super) enum ApprovalPostureChoice {
    Value {
        label: &'static str,
        value: ApprovalPosture,
    },
    Reset,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ApprovalPosturePicker {
    rows: Vec<ApprovalPostureChoice>,
    selected: usize,
    open: bool,
    window: ListWindow,
}

impl ApprovalPosturePicker {
    pub(super) fn open(&mut self, current: ApprovalPosture) {
        self.rows = rows(current);
        self.selected = self
            .rows
            .iter()
            .position(|row| matches!(row, ApprovalPostureChoice::Value { value, .. } if *value == current))
            .unwrap_or(0);
        self.open = true;
        self.window.open();
    }

    pub(super) const fn is_open(&self) -> bool {
        self.open
    }
    pub(super) fn close(&mut self) {
        self.open = false;
    }
    pub(super) fn previous(&mut self) {
        if !self.rows.is_empty() {
            self.selected = self.selected.checked_sub(1).unwrap_or(self.rows.len() - 1);
            self.window.reveal();
        }
    }
    pub(super) fn next(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1) % self.rows.len();
            self.window.reveal();
        }
    }
    pub(super) fn window(&self) -> &ListWindow {
        &self.window
    }
    pub(super) fn rows(&self) -> impl Iterator<Item = (ApprovalPostureChoice, bool)> + '_ {
        self.rows
            .iter()
            .copied()
            .enumerate()
            .map(|(index, row)| (row, index == self.selected))
    }
    pub(super) fn choose(&mut self) -> Option<UpdateApprovalPostureRequest> {
        let request = match *self.rows.get(self.selected)? {
            ApprovalPostureChoice::Value { value, .. } => UpdateApprovalPostureRequest {
                posture: Some(value),
            },
            ApprovalPostureChoice::Reset => UpdateApprovalPostureRequest { posture: None },
        };
        self.close();
        Some(request)
    }
}

fn rows(current: ApprovalPosture) -> Vec<ApprovalPostureChoice> {
    let mut rows = match current {
        ApprovalPosture::Codex {
            approval_policy,
            sandbox_mode,
        } => vec![
            choice(
                "Approval policy: untrusted",
                ApprovalPosture::Codex {
                    approval_policy: CodexApprovalPolicy::Untrusted,
                    sandbox_mode,
                },
            ),
            choice(
                "Approval policy: on-request",
                ApprovalPosture::Codex {
                    approval_policy: CodexApprovalPolicy::OnRequest,
                    sandbox_mode,
                },
            ),
            choice(
                "Approval policy: never",
                ApprovalPosture::Codex {
                    approval_policy: CodexApprovalPolicy::Never,
                    sandbox_mode,
                },
            ),
            choice(
                "Sandbox: read-only",
                ApprovalPosture::Codex {
                    approval_policy,
                    sandbox_mode: CodexSandboxMode::ReadOnly,
                },
            ),
            choice(
                "Sandbox: workspace-write",
                ApprovalPosture::Codex {
                    approval_policy,
                    sandbox_mode: CodexSandboxMode::WorkspaceWrite,
                },
            ),
            choice(
                "Sandbox: danger-full-access",
                ApprovalPosture::Codex {
                    approval_policy,
                    sandbox_mode: CodexSandboxMode::DangerFullAccess,
                },
            ),
        ],
        ApprovalPosture::Claude { .. } => [
            ("Permission mode: default", ClaudePermissionMode::Default),
            (
                "Permission mode: acceptEdits",
                ClaudePermissionMode::AcceptEdits,
            ),
            ("Permission mode: dontAsk", ClaudePermissionMode::DontAsk),
            (
                "Permission mode: bypassPermissions",
                ClaudePermissionMode::BypassPermissions,
            ),
            ("Permission mode: auto", ClaudePermissionMode::Auto),
        ]
        .into_iter()
        .map(|(label, permission_mode)| choice(label, ApprovalPosture::Claude { permission_mode }))
        .collect(),
        ApprovalPosture::Copilot { .. } => vec![
            choice(
                "Permissions: ask",
                ApprovalPosture::Copilot {
                    permissions: CopilotPermissions::Ask,
                },
            ),
            choice(
                "Permissions: allowAll",
                ApprovalPosture::Copilot {
                    permissions: CopilotPermissions::AllowAll,
                },
            ),
        ],
    };
    rows.push(ApprovalPostureChoice::Reset);
    rows
}

const fn choice(label: &'static str, value: ApprovalPosture) -> ApprovalPostureChoice {
    ApprovalPostureChoice::Value { label, value }
}
