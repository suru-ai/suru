//! Agent Selection: the authoritative choice, the Provider's effective one, and
//! the rejection that puts a Session back in the user's hands.

use anyhow::anyhow;

use crate::protocol::{
    Activity, ActivityId, AgentId, AgentIdentity, AgentSelection, ModelAvailability,
    ModelOptionValue, Prompt, PromptDelivery, PromptId, PromptStatus, SessionChange, SessionId,
    SessionUpdate, TurnId, TurnStatus, UpdateAgentSelectionRequest,
};

use super::{
    SessionRecord, SessionStore,
    prompts::{PromptOrigin, PromptOwner},
    settlement::{TrailingCommandOutput, settle_in_flight_changes},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentSelectionMutationError {
    SessionNotFound,
    OperationConflict,
    ProviderConflict,
}

pub(crate) struct AgentSelectionMutation {
    pub(crate) selection: AgentSelection,
    pub(crate) retry_prompt_id: Option<PromptId>,
}

impl SessionStore {
    pub(crate) fn reconcile_effective_agent_selection(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        agent_id: AgentId,
        selection: AgentSelection,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let turn = record
            .snapshot
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .ok_or_else(|| anyhow!("Effective Agent Selection referenced an unknown Turn"))?;
        if turn.status != TurnStatus::Active {
            return Err(anyhow!(
                "Effective Agent Selection referenced a terminal Turn"
            ));
        }
        let requested = turn
            .agent
            .as_ref()
            .ok_or_else(|| anyhow!("Effective Agent Selection referenced an unbound Turn"))?;
        if requested.agent != agent_id || requested.selection.provider != selection.provider {
            return Err(anyhow!(
                "Effective Agent Selection changed the Provider identity of an active Turn"
            ));
        }
        if requested.selection == selection {
            return Ok(None);
        }
        let update_authoritative =
            record.snapshot.session.agent_selection.as_ref() == Some(&requested.selection);
        let effective_agent = AgentIdentity {
            agent: agent_id,
            selection: selection.clone(),
        };
        let mut changes = Vec::new();
        if update_authoritative {
            changes.push(SessionChange::AgentSelectionChanged {
                selection: selection.clone(),
            });
        }
        changes.push(SessionChange::TurnAgentChanged {
            turn_id,
            agent: effective_agent,
        });
        if requested.selection.model != selection.model {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: format!(
                        "Provider used Model `{}` instead of requested Model `{}`.",
                        selection.model, requested.selection.model
                    ),
                },
            });
        }
        for effective in &selection.options {
            let requested_value = requested
                .selection
                .options
                .iter()
                .find(|candidate| candidate.id == effective.id)
                .map(|candidate| &candidate.value);
            if requested_value == Some(&effective.value) {
                continue;
            }
            let requested_value = requested_value
                .map(model_option_value_text)
                .unwrap_or_else(|| "no value".to_owned());
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: format!(
                        "Provider used Model Option `{}` value `{}` instead of requested value `{requested_value}`.",
                        effective.id,
                        model_option_value_text(&effective.value),
                    ),
                },
            });
        }
        for omitted in requested.selection.options.iter().filter(|requested| {
            !selection
                .options
                .iter()
                .any(|effective| effective.id == requested.id)
        }) {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Status {
                    id: ActivityId::new(),
                    turn_id,
                    text: format!(
                        "Provider omitted requested Model Option `{}` value `{}`.",
                        omitted.id,
                        model_option_value_text(&omitted.value),
                    ),
                },
            });
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let update = record.commit(&self.storage, session_id, changes, updated_at)?;
        Ok(Some(update))
    }

    pub(crate) fn initialize_agent_selection(
        &self,
        session_id: SessionId,
        selection: AgentSelection,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?
            .snapshot
            .session
            .agent_selection
            .is_some()
        {
            return Ok(None);
        }
        let updated_at = state.next_timestamp();
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let update = record.commit(
            &self.storage,
            session_id,
            vec![SessionChange::AgentSelectionChanged { selection }],
            updated_at,
        )?;
        Ok(Some(update))
    }

    pub(crate) fn apply_agent_selection_command(
        &self,
        session_id: SessionId,
        request: UpdateAgentSelectionRequest,
    ) -> Result<AgentSelectionMutation, AgentSelectionMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or(AgentSelectionMutationError::SessionNotFound)?;
        if let Some(previous) = record.selection_operations.get(&request.operation_id) {
            return if previous == &request.selection {
                Ok(AgentSelectionMutation {
                    selection: previous.clone(),
                    retry_prompt_id: None,
                })
            } else {
                Err(AgentSelectionMutationError::OperationConflict)
            };
        }
        if record
            .snapshot
            .session
            .agent_selection
            .as_ref()
            .is_some_and(|current| current.provider != request.selection.provider)
        {
            return Err(AgentSelectionMutationError::ProviderConflict);
        }
        let selection_changed =
            record.snapshot.session.agent_selection.as_ref() != Some(&request.selection);
        let availability_changed =
            record.snapshot.session.agent_selection_availability != ModelAvailability::Available;
        let changed = selection_changed || availability_changed;
        let updated_at = changed.then(|| state.next_timestamp());
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        if changed {
            let mut changes = Vec::with_capacity(2);
            if selection_changed {
                changes.push(SessionChange::AgentSelectionChanged {
                    selection: request.selection.clone(),
                });
            }
            if availability_changed {
                changes.push(SessionChange::AgentSelectionAvailabilityChanged {
                    availability: ModelAvailability::Available,
                });
            }
            record
                .commit(
                    &self.storage,
                    session_id,
                    changes,
                    updated_at.expect("changed selection has a timestamp"),
                )
                .expect("Agent Selection commands preserve Session invariants");
        }
        record
            .selection_operations
            .insert(request.operation_id, request.selection.clone());
        Ok(AgentSelectionMutation {
            selection: request.selection,
            retry_prompt_id: record.pending_selection_retry_prompt(),
        })
    }

    pub(crate) fn reject_agent_selection(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        trailing_output: TrailingCommandOutput,
        message: String,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (prompt, mark_unavailable, admission_order, mut changes) = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            let turn = record
                .snapshot
                .turns
                .iter()
                .find(|turn| turn.id == turn_id)
                .ok_or_else(|| anyhow!("Selection rejection referenced an unknown Turn"))?;
            if turn.status != TurnStatus::Active {
                return Err(anyhow!("Selection rejection referenced a terminal Turn"));
            }
            let prompt = record
                .snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == turn.prompt_id)
                .expect("every Turn retains its originating Prompt");
            let mark_unavailable = turn.agent.as_ref().is_some_and(|agent| {
                record.snapshot.session.agent_selection.as_ref() == Some(&agent.selection)
            });
            (
                prompt.clone(),
                mark_unavailable,
                record.next_prompt_order,
                settle_in_flight_changes(&record.snapshot, turn_id, trailing_output),
            )
        };
        let restored = Prompt {
            id: PromptId::new(),
            text: prompt.text,
            delivery: PromptDelivery::Queue,
            admission_order,
            status: PromptStatus::Pending,
        };
        changes.extend([
            SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            },
            SessionChange::TurnStatusChanged {
                turn_id,
                status: TurnStatus::Failed,
            },
        ]);
        if mark_unavailable {
            changes.push(SessionChange::AgentSelectionAvailabilityChanged {
                availability: ModelAvailability::Unavailable,
            });
        }
        changes.push(SessionChange::PromptAdded {
            prompt: restored.clone(),
        });
        let updated_at = state.next_timestamp();
        let update = {
            let record = state
                .sessions
                .get_mut(&session_id)
                .expect("Session existence was checked while holding the store lock");
            let update = record.commit(&self.storage, session_id, changes, updated_at)?;
            record.selection_retry_prompt = Some(restored.id);
            update
        };
        state.prompts.insert(
            restored.id,
            PromptOwner {
                session_id,
                text: restored.text,
                agent_selection: None,
                origin: PromptOrigin::Admission(restored.delivery),
            },
        );
        Ok(update)
    }
}

impl SessionRecord {
    fn pending_selection_retry_prompt(&self) -> Option<PromptId> {
        let prompt_id = self.selection_retry_prompt?;
        self.snapshot
            .prompts
            .iter()
            .any(|prompt| prompt.id == prompt_id && prompt.status == PromptStatus::Pending)
            .then_some(prompt_id)
    }
}

fn model_option_value_text(value: &ModelOptionValue) -> String {
    match value {
        ModelOptionValue::Select { choice } => choice.to_string(),
        ModelOptionValue::Toggle { enabled } => enabled.to_string(),
    }
}
