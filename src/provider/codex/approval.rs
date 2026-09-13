//! Correlation between Suru Approval identities and Codex JSON-RPC callbacks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};

use super::{codex_error, wire::RequestId};
use crate::{
    protocol::{ApprovalId, Decision},
    provider::ProviderError,
};

#[derive(Clone, Default)]
pub(super) struct CodexApprovals(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    pending: HashMap<ApprovalId, NativeApproval>,
}

#[derive(Clone)]
pub(super) struct NativeApprovalIdentity {
    pub(super) request_id: RequestId,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) kind: NativeApprovalKind,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct NativeApprovalTurn {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
}

#[derive(Clone, Debug)]
pub(super) struct NativeInterruptTarget {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
}

#[derive(Clone)]
pub(super) enum NativeApprovalKind {
    Command,
    FileChange,
    Permissions { requested: Value },
}

struct NativeApproval {
    identity: NativeApprovalIdentity,
}

pub(super) struct NativeDecision {
    pub(super) request_id: RequestId,
    pub(super) result: Value,
    pub(super) turn: NativeApprovalTurn,
    pub(super) interrupt: Option<NativeInterruptTarget>,
}

impl CodexApprovals {
    pub(super) fn register(
        &self,
        identity: NativeApprovalIdentity,
    ) -> Result<ApprovalId, ProviderError> {
        let mut state = self.0.lock().expect("Codex Approval lock is not poisoned");
        if state
            .pending
            .values()
            .any(|approval| approval.identity.request_id == identity.request_id)
        {
            return Err(codex_error(
                "Codex reused a live Approval callback identity",
            ));
        }
        let id = ApprovalId::new();
        state.pending.insert(id, NativeApproval { identity });
        Ok(id)
    }

    pub(super) fn take_decision(
        &self,
        id: ApprovalId,
        decision: Decision,
    ) -> Result<NativeDecision, ProviderError> {
        let native = self
            .0
            .lock()
            .expect("Codex Approval lock is not poisoned")
            .pending
            .remove(&id)
            .ok_or_else(|| ProviderError::decision_rejected("Codex Approval is unavailable"))?;
        let (result, interrupt) = match native.identity.kind {
            NativeApprovalKind::Command | NativeApprovalKind::FileChange => (
                json!({
                    "decision": match decision {
                        Decision::Accept => "accept",
                        Decision::AcceptForSession => "acceptForSession",
                        Decision::Decline => "decline",
                        Decision::DeclineAndInterrupt => "cancel",
                    }
                }),
                None,
            ),
            NativeApprovalKind::Permissions { requested } => (
                json!({
                    "permissions": if matches!(decision, Decision::Accept | Decision::AcceptForSession) {
                        requested
                    } else {
                        json!({})
                    },
                    // Codex's permission callback offers a scope rather than the
                    // command/file decision enum. Suru grants only this Turn.
                    "scope": "turn",
                }),
                matches!(decision, Decision::DeclineAndInterrupt).then(|| NativeInterruptTarget {
                    thread_id: native.identity.thread_id.clone(),
                    turn_id: native.identity.turn_id.clone(),
                }),
            ),
        };
        Ok(NativeDecision {
            request_id: native.identity.request_id,
            result,
            turn: NativeApprovalTurn {
                thread_id: native.identity.thread_id,
                turn_id: native.identity.turn_id,
            },
            interrupt,
        })
    }

    pub(super) fn resolve(&self, request_id: &RequestId) -> Option<ApprovalId> {
        self.remove_where(|approval| &approval.identity.request_id == request_id)
            .into_iter()
            .next()
    }

    pub(super) fn end_turn(&self, thread_id: &str, turn_id: &str) -> Vec<ApprovalId> {
        self.remove_where(|approval| {
            approval.identity.thread_id == thread_id && approval.identity.turn_id == turn_id
        })
    }

    pub(super) fn end_thread(&self, thread_id: &str) -> Vec<ApprovalId> {
        self.remove_where(|approval| approval.identity.thread_id == thread_id)
    }

    pub(super) fn clear(&self) {
        self.remove_where(|_| true);
    }

    fn remove_where(&self, predicate: impl Fn(&NativeApproval) -> bool) -> Vec<ApprovalId> {
        let mut state = self.0.lock().expect("Codex Approval lock is not poisoned");
        let mut removed = Vec::new();
        state.pending.retain(|id, approval| {
            if predicate(approval) {
                removed.push(*id);
                false
            } else {
                true
            }
        });
        removed
    }
}
