//! Prompt admission, ordering, delivery, and the queue mutations a client drives.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use tokio::sync::broadcast;

use crate::protocol::{
    Activity, ActivityId, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection,
    AttachmentBinding, AttachmentDescriptor, CreateSessionRequest, Message, MessageId, MessageRole,
    MessageStatus, ModelAvailability, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus,
    Session, SessionCatalogChange, SessionChange, SessionId, SessionRevision, SessionSnapshot,
    SessionStatus, SessionSummary, SessionUpdate, SkillInvocation, Turn, TurnId, TurnStatus,
};

use crate::storage::{PersistedSession, StorageSink};

use super::{
    SESSION_UPDATE_CAPACITY, SessionRecord, SessionStore, SessionStoreState, StoreOutcome,
    projection::active_turn_id,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreateSessionError {
    EmptyPrompt,
    InvalidWorkspace,
    PromptConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmitPromptError {
    EmptyPrompt,
    SessionNotFound,
    /// The Session is a Subagent's, and a Subagent's Session is never offered
    /// a Prompt: its conversation is the Provider's to drive.
    SubagentSession,
    PromptConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptMutationError {
    SessionNotFound,
    PromptNotFound,
    PromptNotPending,
}

pub(crate) struct PromptAdmission {
    pub(crate) prompt: Prompt,
    pub(crate) disposition: PromptAdmissionDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptAdmissionDisposition {
    StartImmediately,
    SteerActive,
    RemainPending,
}

/// What starting a delivered Turn with its Provider takes. It names the Turn
/// rather than carrying a copy of it: the commit that delivers a Turn stamps
/// when it started, so a copy taken before that commit would disagree with the
/// Turn every other reader sees.
pub(crate) struct DeliveredTurn {
    pub(crate) prompt: Prompt,
    pub(crate) turn_id: TurnId,
    pub(crate) agent: Option<AgentIdentity>,
}

pub(crate) enum DeliveredTurnStatus {
    Active,
    Failed { message: String },
}

pub(crate) fn earliest_pending_prompt<'a>(
    prompts: impl IntoIterator<Item = &'a Prompt>,
    delivery: PromptDelivery,
) -> Option<&'a Prompt> {
    prompts
        .into_iter()
        .filter(|prompt| prompt.status == PromptStatus::Pending && prompt.delivery == delivery)
        .min_by_key(|prompt| prompt.admission_order)
}

/// The Session a Prompt belongs to, alongside everything a retry of the request
/// that introduced it must match to be recognized as the same Prompt rather
/// than a conflicting one.
pub(super) struct PromptOwner {
    pub(super) session_id: SessionId,
    pub(super) text: String,
    pub(super) skill_invocations: Vec<SkillInvocation>,
    pub(super) attachments: Vec<AttachmentBinding>,
    pub(super) agent_selection: Option<AgentSelection>,
    pub(super) origin: PromptOrigin,
}

pub(super) enum PromptOrigin {
    SessionCreation {
        requested_execution_directory: PathBuf,
        canonical_execution_directory: PathBuf,
    },
    Admission(PromptDelivery),
}

#[derive(Clone, Copy)]
enum CreationRetry {
    Ordinary,
    Prepared,
}

/// The delivery a Prompt admitted right now would actually receive.
///
/// A Steer joins the active Turn, but a Session running no Turn has nothing
/// to join, and a Continuation is settled by the next delivered Prompt rather
/// than steered. In both cases the Prompt begins a Turn of its own, which is a
/// queued delivery whatever the client asked for. Skill validation must judge
/// that effective delivery: a Provider that steers no Skills still starts
/// them.
pub(crate) fn effective_delivery(
    snapshot: &SessionSnapshot,
    requested: PromptDelivery,
) -> PromptDelivery {
    if requested == PromptDelivery::Steer && steerable_turn(snapshot).is_some() {
        PromptDelivery::Steer
    } else {
        PromptDelivery::Queue
    }
}

/// The active Turn a steer would join, if the Session is running one.
///
/// A Continuation is settled by the next delivered Prompt rather than
/// steered, so it is never the answer.
fn steerable_turn(snapshot: &SessionSnapshot) -> Option<TurnId> {
    let turn_id = active_turn_id(snapshot).ok().flatten()?;
    snapshot
        .turns
        .iter()
        .find(|turn| turn.id == turn_id && !turn.is_continuation())
        .map(|turn| turn.id)
}

impl SessionStoreState {
    /// Withdraws every Prompt that was admitted to begin a Turn and persisted
    /// still waiting for it. A Prompt is owed its Turn by the process that
    /// admitted it — nothing durable carries that debt across a restart, and
    /// no reader can be asked to wait on an Agent that will never be asked to
    /// answer — so restoring one is withdrawing it, exactly as interrupting it
    /// would have (ADR 0024). A Prompt waiting behind a Turn in the queue is
    /// not one of these: its delivery is the Session's own to make when that
    /// Turn settles.
    ///
    /// The exception is a Session a stored Worktree preparation can still
    /// bring to its first Turn: that intention is the durable record of a
    /// Prompt still owed a Turn, and rejoining the preparation is what
    /// delivers it. Once the preparation has recorded the Session it admitted,
    /// the delivery was this process's to make and nothing survives it.
    ///
    /// Deferred Sessions are passed over rather than reported: their histories
    /// have not been read yet, and hydrating one is what brings it here.
    pub(super) fn withdraw_stranded_prompts(
        &mut self,
        storage: &StorageSink,
        sessions: impl IntoIterator<Item = SessionId>,
    ) {
        for session_id in sessions {
            if self.is_deferred(session_id) || self.resumable_preparations.contains(&session_id) {
                continue;
            }
            let Some(record) = self.sessions.get(&session_id) else {
                continue;
            };
            let stranded = record
                .snapshot
                .prompts
                .iter()
                .filter(|prompt| {
                    prompt.status == PromptStatus::Pending
                        && prompt.delivery == PromptDelivery::Steer
                        && !record
                            .snapshot
                            .turns
                            .iter()
                            .any(|turn| turn.prompt_id == Some(prompt.id))
                })
                .map(|prompt| SessionChange::PromptStatusChanged {
                    prompt_id: prompt.id,
                    status: PromptStatus::Cancelled,
                })
                .collect::<Vec<_>>();
            if stranded.is_empty() {
                continue;
            }
            if let Err(error) = self.commit(storage, session_id, stranded) {
                tracing::warn!("Restored Prompt could not be withdrawn: {error}");
            }
        }
    }
}

impl SessionStore {
    /// Reads the first line of the one durable delivery promise carried by an
    /// unadmitted Worktree preparation for the Reclaim audit log.
    pub(crate) async fn preparation_prompt_first_line(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, String> {
        self.hydrate(session_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(self.snapshot(session_id).and_then(|snapshot| {
            withheld_preparation_prompt(&snapshot)
                .map(|prompt| prompt.text.lines().next().unwrap_or_default().to_owned())
        }))
    }

    /// Ends that delivery promise while preserving the Session and its Prompt
    /// history. Retirement is durable because cancellation is committed before
    /// the preparation intent is deleted.
    pub(crate) async fn retire_preparation_prompt(
        &self,
        session_id: SessionId,
    ) -> Result<(), String> {
        self.hydrate(session_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let prompt = state
            .sessions
            .get(&session_id)
            .and_then(|record| withheld_preparation_prompt(&record.snapshot))
            .cloned();
        if let Some(prompt) = &prompt {
            state
                .commit(
                    &self.storage,
                    session_id,
                    vec![SessionChange::PromptStatusChanged {
                        prompt_id: prompt.id,
                        status: PromptStatus::Cancelled,
                    }],
                )
                .map_err(|error| error.to_string())?;
        }
        state.resumable_preparations.remove(&session_id);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn create(
        &self,
        request: CreateSessionRequest,
    ) -> Result<StoreOutcome<SessionSnapshot>, CreateSessionError> {
        let path = crate::paths::canonical(&request.execution_directory.path)
            .unwrap_or_else(|_| request.execution_directory.path.clone());
        self.create_in(
            request,
            crate::protocol::ResolvedWorkspace::directory(path),
            Vec::new(),
        )
    }

    /// Creates a Session whose first Prompt binds the Attachments `described`
    /// describes: what admission answered for each binding the Prompt carries.
    pub(crate) fn create_in(
        &self,
        request: CreateSessionRequest,
        location: crate::protocol::ResolvedWorkspace,
        described: Vec<AttachmentDescriptor>,
    ) -> Result<StoreOutcome<SessionSnapshot>, CreateSessionError> {
        self.create_in_with_identity(request, location, described, None)
    }

    /// Finds the Session an already admitted creation request made. This is
    /// the idempotency record once a completed Worktree preparation no longer
    /// needs its durable intent.
    pub(crate) fn existing_creation(
        &self,
        request: &CreateSessionRequest,
    ) -> Result<Option<SessionSnapshot>, CreateSessionError> {
        self.existing_creation_with(request, CreationRetry::Ordinary)
    }

    /// Finds an admitted Session after its Worktree preparation intent has
    /// been deleted. Provider discovery may have resolved an implicit Agent
    /// Selection differently since admission, so the original Prompt content
    /// and execution location remain the durable idempotency facts here.
    pub(crate) fn existing_prepared_creation(
        &self,
        request: &CreateSessionRequest,
    ) -> Result<Option<SessionSnapshot>, CreateSessionError> {
        self.existing_creation_with(request, CreationRetry::Prepared)
    }

    fn existing_creation_with(
        &self,
        request: &CreateSessionRequest,
        retry: CreationRetry,
    ) -> Result<Option<SessionSnapshot>, CreateSessionError> {
        let canonical = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let Some(owner) = state.prompts.get(&request.prompt.id) else {
                return Ok(None);
            };
            if owner.text != request.prompt.text
                || owner.skill_invocations != request.prompt.skill_invocations
                || owner.attachments != request.prompt.attachments
                || matches!(retry, CreationRetry::Ordinary)
                    && owner.agent_selection != request.agent_selection
            {
                return Err(CreateSessionError::PromptConflict);
            }
            match &owner.origin {
                PromptOrigin::SessionCreation {
                    requested_execution_directory,
                    ..
                } if requested_execution_directory == &request.execution_directory.path => {
                    return Ok(Some(snapshot_for_owner(&state, owner)));
                }
                PromptOrigin::SessionCreation {
                    canonical_execution_directory,
                    ..
                } => canonical_execution_directory.clone(),
                PromptOrigin::Admission(_) if matches!(retry, CreationRetry::Prepared) => {
                    let record = state
                        .sessions
                        .get(&owner.session_id)
                        .expect("Prompt owner always references its Session");
                    if !(record.snapshot.session.parent.is_none()
                        && record.snapshot.prompts.iter().any(|prompt| {
                            prompt.id == request.prompt.id
                                && prompt.admission_order == PromptOrder::INITIAL
                        }))
                    {
                        return Err(CreateSessionError::PromptConflict);
                    }
                    record.snapshot.session.execution_directory.path.clone()
                }
                PromptOrigin::Admission(_) => return Err(CreateSessionError::PromptConflict),
            }
        };
        let execution_path = crate::paths::canonical(&request.execution_directory.path)
            .map_err(|_| CreateSessionError::PromptConflict)?;
        if execution_path != canonical {
            return Err(CreateSessionError::PromptConflict);
        }
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let owner = state
            .prompts
            .get(&request.prompt.id)
            .expect("Prompt owner remains indexed for the server lifetime");
        Ok(Some(snapshot_for_owner(&state, owner)))
    }

    pub(crate) fn persist_prepared_session(&self, id: SessionId) -> anyhow::Result<()> {
        let state = self.state.lock().unwrap();
        let record = state
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("Prepared Session no longer exists"))?;
        // Observation or grouping may have advanced metadata while destination
        // validation awaited. Persist the current record under its store lock.
        self.storage
            .location_changed(record.snapshot.session.clone(), record.snapshot.revision)?;
        Ok(())
    }

    pub(crate) fn create_in_with_identity(
        &self,
        request: CreateSessionRequest,
        mut location: crate::protocol::ResolvedWorkspace,
        described: Vec<AttachmentDescriptor>,
        intended_session: Option<SessionId>,
    ) -> Result<StoreOutcome<SessionSnapshot>, CreateSessionError> {
        if request.prompt.text.trim().is_empty() {
            return Err(CreateSessionError::EmptyPrompt);
        }

        // A Prompt id is claimed for the life of the server, so a retry always
        // finds the Session that Prompt already made — including one whose
        // Prompt was withdrawn by an interrupt, which answers with that
        // Session and its Cancelled Prompt rather than making a second Session
        // or starting the withdrawn work again. Asking for that work again is
        // a new Prompt, which is what a client's retry after a withdrawal
        // submits (ADR 0024).
        if let Some(snapshot) = self.existing_creation(&request)? {
            return Ok(StoreOutcome::Existing(snapshot));
        }

        let execution_path = crate::paths::canonical(&request.execution_directory.path)
            .map_err(|_| CreateSessionError::InvalidWorkspace)?;
        if !execution_path.is_dir() {
            return Err(CreateSessionError::InvalidWorkspace);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.matches_canonical_creation(&request, &execution_path) {
                return Ok(StoreOutcome::Existing(snapshot_for_owner(&state, owner)));
            }
            return Err(CreateSessionError::PromptConflict);
        }

        // Discovery and Skill validation happen before admission. A path may
        // have been retargeted meanwhile; never pair another directory's
        // Repository association with the directory the Agent will execute in.
        if location
            .execution_directory
            .as_ref()
            .map(|directory| &directory.path)
            != Some(&execution_path)
        {
            return Err(CreateSessionError::InvalidWorkspace);
        }

        // The table is authoritative for a Workspace's Icon; a freshly
        // resolved `location.workspace` never carries one of its own, so this
        // is the one place a newly created Session's Workspace picks it up.
        // It is what lets a second Session in an already-Iconed Workspace skip
        // the Workspace Errand outright, its own `workspace.icon` already
        // `Some` the moment `Derivation::derive` looks at it.
        location.workspace.icon = state.workspace_icons.get(&location.workspace.id).cloned();
        let title = request.prompt.text.trim().to_owned();
        let session_id = intended_session.unwrap_or_default();
        // The Session is Working from this moment: its Prompt is admitted to
        // begin a Turn, and the elapsed time every surface reads is attributed
        // from here rather than from whenever the Provider answers (ADR 0024).
        let timestamp = state.next_timestamp();
        let prompt = Prompt {
            id: request.prompt.id,
            text: request.prompt.text.clone(),
            skill_invocations: request.prompt.skill_invocations.clone(),
            attachments: request.prompt.attachments.clone(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Pending,
        };
        let prompt_id = prompt.id;
        let mut attachments = Vec::with_capacity(described.len());
        crate::session_projection::describe_attachments(&mut attachments, described);
        let snapshot = SessionSnapshot {
            title: title.clone(),
            // A Session begins with none: the Icon beside its Title arrives
            // only once an Errand has derived one, and a Session that never
            // gets one draws just as cleanly.
            icon: None,
            session: Session {
                checkout: location.checkout,
                context_fill: None,
                id: session_id,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_path.clone(),
                },
                workspace: location.workspace,
                agent_selection: request.agent_selection.clone(),
                agent_selection_availability: ModelAvailability::Available,
                approval_posture: None,
                status: SessionStatus::Active,
                working_since: Some(timestamp),
                monitoring_since: None,
                parent: None,
            },
            revision: SessionRevision::INITIAL,
            prompts: vec![prompt],
            turns: Vec::new(),
            messages: Vec::new(),
            activities: Vec::new(),
            transcript: Vec::new(),
            // A Session begins having delegated nothing, so there is nothing
            // below it to roll up.
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            waiting_on_subagents: None,
            subagent_usage: None,
            total_cost: None,
            own_cost: None,
            attachments,
        };
        let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
        let summary = SessionSummary {
            checkout_state: None,
            session: snapshot.session.clone(),
            title,
            icon: None,
            // And it begins active: a Session created by a Prompt is work
            // beginning, which is the opposite of work set aside.
            settled_at: None,
            standing_inputs: Default::default(),
            // And with nothing consumed: no Turn has run to report anything.
            total_usage: None,
            own_cost: None,
            created_at: timestamp,
            updated_at: timestamp,
        };
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
                skill_invocations: request.prompt.skill_invocations,
                attachments: request.prompt.attachments,
                agent_selection: request.agent_selection,
                origin: PromptOrigin::SessionCreation {
                    requested_execution_directory: request.execution_directory.path,
                    canonical_execution_directory: execution_path,
                },
            },
        );
        let persisted_summary = summary.clone();
        state.sessions.insert(
            session_id,
            SessionRecord {
                context_fill_order: None,
                snapshot: snapshot.clone(),
                summary,
                updates,
                next_prompt_order: PromptOrder(2),
                steer_targets: HashMap::new(),
                turn_start_admissions: HashMap::from([(prompt_id, timestamp)]),
                selection_operations: HashMap::new(),
                viewed_operations: Default::default(),
                selection_retry_prompt: None,
                resume_states: HashMap::new(),
                subagent_identity: None,
                // A top-level Session owns its actor by having no parent.
                brokered: false,
                watches: HashMap::new(),
                subagent_waits: Vec::new(),
                work_interrupted_at: None,
                stopped_by_ancestor: None,
                held_reports: Default::default(),
            },
        );
        self.storage.created(PersistedSession::created(
            persisted_summary,
            snapshot.clone(),
        ));
        state.publish_catalog_change(SessionCatalogChange::Created { session_id });
        Ok(StoreOutcome::Created(snapshot))
    }

    /// Admits a Prompt binding the Attachments `described` describes: what
    /// admission answered for each binding the Prompt carries.
    pub(crate) fn admit(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
        described: Vec<AttachmentDescriptor>,
    ) -> Result<StoreOutcome<PromptAdmission>, AdmitPromptError> {
        if request.prompt.text.trim().is_empty() {
            return Err(AdmitPromptError::EmptyPrompt);
        }

        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(owner) = state.prompts.get(&request.prompt.id) {
            if owner.matches_admission(session_id, &request) {
                let prompt = state
                    .sessions
                    .get(&session_id)
                    .and_then(|record| {
                        record
                            .snapshot
                            .prompts
                            .iter()
                            .find(|prompt| prompt.id == request.prompt.id)
                    })
                    .expect("Prompt owner always references its Prompt")
                    .clone();
                return Ok(StoreOutcome::Existing(PromptAdmission {
                    prompt,
                    disposition: PromptAdmissionDisposition::RemainPending,
                }));
            }
            return Err(AdmitPromptError::PromptConflict);
        }

        match state.sessions.get(&session_id) {
            None => return Err(AdmitPromptError::SessionNotFound),
            Some(record) if record.snapshot.session.is_subagent() => {
                return Err(AdmitPromptError::SubagentSession);
            }
            Some(_) => {}
        }
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        let admission_order = record.next_prompt_order;
        let next_prompt_order = PromptOrder(
            admission_order
                .0
                .checked_add(1)
                .expect("Prompt admission order space is not exhausted"),
        );
        let active_turn = active_turn_id(&record.snapshot)
            .expect("stored Sessions preserve the one-active-Turn invariant");
        let (disposition, steer_target) = match (active_turn, request.delivery) {
            (None, _) => (PromptAdmissionDisposition::StartImmediately, None),
            // A steer with no steerable Turn to join begins a Turn of its own
            // rather than waiting behind the Continuation that is running.
            (Some(_), PromptDelivery::Steer) => match steerable_turn(&record.snapshot) {
                Some(turn_id) => (PromptAdmissionDisposition::SteerActive, Some(turn_id)),
                None => (PromptAdmissionDisposition::StartImmediately, None),
            },
            (Some(_), PromptDelivery::Queue) => (PromptAdmissionDisposition::RemainPending, None),
        };
        let prompt = Prompt {
            id: request.prompt.id,
            text: request.prompt.text.clone(),
            skill_invocations: request.prompt.skill_invocations.clone(),
            attachments: request.prompt.attachments.clone(),
            delivery: request.delivery,
            admission_order,
            status: PromptStatus::Pending,
        };
        // Work has arrived for this Session, so it is no longer set aside. The
        // marker goes before the commit, so the summary that commit persists is
        // the active one and the announcement follows the Prompt it belongs to.
        let reactivation = record.reactivate(session_id);
        // An admission owed its own Turn begins Working with the commit that
        // admits it, so the commit stamps the admission moment it derives
        // that reading from (ADR 0024).
        let turn_start = matches!(disposition, PromptAdmissionDisposition::StartImmediately)
            .then_some(prompt.id);
        // Every client learns what an Attachment is before any Prompt binds
        // it, and only once for the life of the Session.
        let undescribed = described
            .into_iter()
            .filter(|descriptor| record.snapshot.attachment(&descriptor.id).is_none())
            .collect::<Vec<_>>();
        let mut changes = Vec::with_capacity(2);
        if !undescribed.is_empty() {
            changes.push(SessionChange::AttachmentsDescribed {
                attachments: undescribed,
            });
        }
        changes.push(SessionChange::PromptAdded {
            prompt: prompt.clone(),
        });
        state
            .commit_admission(&self.storage, session_id, changes, turn_start)
            .expect("admission changes preserve Session invariants");
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.next_prompt_order = next_prompt_order;
        if let Some(turn_id) = steer_target {
            record.steer_targets.insert(prompt.id, turn_id);
        }
        state.prompts.insert(
            request.prompt.id,
            PromptOwner {
                session_id,
                text: request.prompt.text,
                skill_invocations: request.prompt.skill_invocations,
                attachments: request.prompt.attachments,
                agent_selection: None,
                origin: PromptOrigin::Admission(request.delivery),
            },
        );
        if let Some(change) = reactivation {
            state.publish_catalog_change(change);
        }
        Ok(StoreOutcome::Created(PromptAdmission {
            prompt,
            disposition,
        }))
    }

    pub(crate) fn deliver_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
        agent_id: Option<AgentId>,
        delivered_turn_status: DeliveredTurnStatus,
    ) -> anyhow::Result<Option<DeliveredTurn>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (prompt, agent) = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)?.is_some() {
                return Ok(None);
            }
            let prompt = record
                .snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == prompt_id)
                .ok_or_else(|| anyhow!("Prompt delivery referenced an unknown Prompt"))?;
            if prompt.status != PromptStatus::Pending {
                return Ok(None);
            }
            let agent = agent_id.and_then(|agent| {
                record
                    .snapshot
                    .session
                    .agent_selection
                    .clone()
                    .map(|selection| AgentIdentity { agent, selection })
            });
            (prompt.clone(), agent)
        };
        let (turn_status, failure_message) = match delivered_turn_status {
            DeliveredTurnStatus::Active => (TurnStatus::Active, None),
            DeliveredTurnStatus::Failed { message } => (TurnStatus::Failed, Some(message)),
        };
        let (delivered, mut changes) = prepare_prompt_delivery(prompt, agent, turn_status);
        if let Some(message) = failure_message {
            changes.push(SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id: delivered.turn_id,
                    text: message,
                },
            });
        }
        state.commit(&self.storage, session_id, changes)?;
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.steer_targets.remove(&prompt_id);
        if record.selection_retry_prompt == Some(prompt_id) {
            record.selection_retry_prompt = None;
        }
        Ok(Some(delivered))
    }

    pub(crate) fn next_pending_steer(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<Option<Prompt>> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        record.next_pending_steer(turn_id)
    }

    pub(crate) fn deliver_steer(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        prompt_id: PromptId,
    ) -> anyhow::Result<Option<Prompt>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let prompt = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            let Some(prompt) = record.pending_steer(turn_id, prompt_id)? else {
                return Ok(None);
            };
            let earliest = record
                .next_pending_steer(turn_id)?
                .expect("the target is a pending steer Prompt");
            if earliest.id != prompt_id {
                return Err(anyhow!(
                    "Steer Prompts must be delivered in admission order"
                ));
            }
            prompt.clone()
        };
        let mut changes = Vec::with_capacity(2);
        append_steer_delivery_changes(&mut changes, &prompt, turn_id);
        state.commit(&self.storage, session_id, changes)?;
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.steer_targets.remove(&prompt_id);
        let mut delivered = prompt;
        delivered.status = PromptStatus::Delivered;
        Ok(Some(delivered))
    }

    pub(crate) fn report_steer_failure(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        prompt_id: PromptId,
        message: String,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if record.pending_steer(turn_id, prompt_id)?.is_none() {
                return Ok(None);
            }
        }
        let update = state.commit(
            &self.storage,
            session_id,
            vec![SessionChange::ActivityAdded {
                activity: Activity::Error {
                    id: ActivityId::new(),
                    turn_id,
                    text: message,
                },
            }],
        )?;
        state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock")
            .steer_targets
            .remove(&prompt_id);
        Ok(Some(update))
    }

    /// Records a Skill-bearing steer that failed before or at native delivery.
    /// Unlike an ordinary transient steer failure, this Prompt cannot be
    /// retried under weaker plain-text semantics: its durable Message and safe
    /// bindings remain beside the visible Error, while its terminal status
    /// keeps the continuation boundary from later claiming it was delivered.
    pub(crate) fn fail_skill_steer(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        prompt_id: PromptId,
        message: String,
    ) -> anyhow::Result<Option<SessionUpdate>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let prompt = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            let Some(prompt) = record.pending_steer(turn_id, prompt_id)? else {
                return Ok(None);
            };
            prompt.clone()
        };
        let update = state.commit(
            &self.storage,
            session_id,
            vec![
                SessionChange::PromptStatusChanged {
                    prompt_id,
                    status: PromptStatus::Failed,
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: prompt.text,
                        skill_invocations: prompt.skill_invocations,
                        attachments: prompt.attachments,
                        truncated: false,
                    },
                },
                SessionChange::ActivityAdded {
                    activity: Activity::Error {
                        id: ActivityId::new(),
                        turn_id,
                        text: message,
                    },
                },
            ],
        )?;
        state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock")
            .steer_targets
            .remove(&prompt_id);
        Ok(Some(update))
    }

    pub(crate) fn continuation_boundary(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<Vec<Prompt>> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let delivered = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
            if active_turn_id(&record.snapshot)? != Some(turn_id) {
                return Err(anyhow!("Turn is not the active Turn for this Session"));
            }
            let mut prompts = record
                .snapshot
                .prompts
                .iter()
                .filter(|prompt| {
                    prompt.status == PromptStatus::Pending
                        && prompt.delivery == PromptDelivery::Steer
                        && !record.turn_start_admissions.contains_key(&prompt.id)
                })
                .cloned()
                .collect::<Vec<_>>();
            prompts.sort_unstable_by_key(|prompt| prompt.admission_order);
            prompts
        };
        if delivered.is_empty() {
            return Ok(delivered);
        }

        let mut changes = Vec::with_capacity(delivered.len() * 2);
        for prompt in &delivered {
            changes.extend([
                SessionChange::PromptStatusChanged {
                    prompt_id: prompt.id,
                    status: PromptStatus::Delivered,
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: prompt.text.clone(),
                        skill_invocations: prompt.skill_invocations.clone(),
                        attachments: prompt.attachments.clone(),
                        truncated: false,
                    },
                },
            ]);
        }
        state.commit(&self.storage, session_id, changes)?;
        Ok(delivered
            .into_iter()
            .map(|mut prompt| {
                prompt.status = PromptStatus::Delivered;
                prompt
            })
            .collect())
    }

    pub(crate) fn promote(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt, PromptMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut prompt = {
            let record = state
                .sessions
                .get(&session_id)
                .ok_or(PromptMutationError::SessionNotFound)?;
            let prompt = record
                .snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == prompt_id)
                .ok_or(PromptMutationError::PromptNotFound)?
                .clone();
            if prompt.status != PromptStatus::Pending {
                return Err(PromptMutationError::PromptNotPending);
            }
            if prompt.delivery == PromptDelivery::Steer {
                return Ok(prompt);
            }
            prompt
        };
        prompt.delivery = PromptDelivery::Steer;
        state
            .commit(
                &self.storage,
                session_id,
                vec![SessionChange::PromptDeliveryChanged {
                    prompt_id,
                    delivery: PromptDelivery::Steer,
                }],
            )
            .expect("Prompt promotion preserves Session invariants");
        Ok(prompt)
    }

    pub(crate) fn cancel(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
    ) -> Result<Prompt, PromptMutationError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut prompt = state
            .sessions
            .get(&session_id)
            .ok_or(PromptMutationError::SessionNotFound)?
            .snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .ok_or(PromptMutationError::PromptNotFound)?
            .clone();
        if prompt.status == PromptStatus::Cancelled {
            return Ok(prompt);
        }
        if prompt.status != PromptStatus::Pending || prompt.delivery != PromptDelivery::Queue {
            return Err(PromptMutationError::PromptNotPending);
        }
        state
            .commit(
                &self.storage,
                session_id,
                vec![SessionChange::PromptStatusChanged {
                    prompt_id,
                    status: PromptStatus::Cancelled,
                }],
            )
            .expect("Prompt cancellation preserves Session invariants");
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        if record.selection_retry_prompt == Some(prompt_id) {
            record.selection_retry_prompt = None;
        }
        prompt.status = PromptStatus::Cancelled;
        Ok(prompt)
    }
}

fn withheld_preparation_prompt(snapshot: &SessionSnapshot) -> Option<&Prompt> {
    if !snapshot.turns.is_empty() {
        return None;
    }
    snapshot.prompts.iter().find(|prompt| {
        prompt.status == PromptStatus::Pending && prompt.delivery == PromptDelivery::Steer
    })
}

impl SessionRecord {
    fn next_pending_steer(&self, turn_id: TurnId) -> anyhow::Result<Option<Prompt>> {
        if active_turn_id(&self.snapshot)? != Some(turn_id) {
            return Ok(None);
        }
        Ok(self
            .snapshot
            .prompts
            .iter()
            .filter(|prompt| {
                prompt.status == PromptStatus::Pending
                    && prompt.delivery == PromptDelivery::Steer
                    && self.steer_targets.get(&prompt.id) == Some(&turn_id)
            })
            .min_by_key(|prompt| prompt.admission_order)
            .cloned())
    }

    fn pending_steer(
        &self,
        turn_id: TurnId,
        prompt_id: PromptId,
    ) -> anyhow::Result<Option<Prompt>> {
        if active_turn_id(&self.snapshot)? != Some(turn_id) {
            return Ok(None);
        }
        let prompt = self
            .snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .ok_or_else(|| anyhow!("Steering referenced an unknown Prompt"))?;
        if prompt.status != PromptStatus::Pending {
            return Ok(None);
        }
        if prompt.delivery != PromptDelivery::Steer {
            return Err(anyhow!("Steering referenced a queued Prompt"));
        }
        if self.steer_targets.get(&prompt_id) != Some(&turn_id) {
            return Err(anyhow!(
                "Steering referenced a Prompt admitted outside the active Turn"
            ));
        }
        Ok(Some(prompt.clone()))
    }
}

pub(super) fn append_steer_delivery_changes(
    changes: &mut Vec<SessionChange>,
    prompt: &Prompt,
    turn_id: TurnId,
) {
    changes.extend([
        SessionChange::PromptStatusChanged {
            prompt_id: prompt.id,
            status: PromptStatus::Delivered,
        },
        SessionChange::MessageAdded {
            message: Message {
                id: MessageId::new(),
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: prompt.text.clone(),
                skill_invocations: prompt.skill_invocations.clone(),
                attachments: prompt.attachments.clone(),
                truncated: false,
            },
        },
    ]);
}

pub(super) fn prepare_prompt_delivery(
    prompt: Prompt,
    agent: Option<AgentIdentity>,
    turn_status: TurnStatus,
) -> (DeliveredTurn, Vec<SessionChange>) {
    let turn_id = TurnId::new();
    let changes = vec![
        SessionChange::PromptStatusChanged {
            prompt_id: prompt.id,
            status: PromptStatus::Delivered,
        },
        SessionChange::TurnAdded {
            turn: Turn {
                id: turn_id,
                prompt_id: Some(prompt.id),
                agent: agent.clone(),
                status: turn_status,
                // The commit that lands this delivery stamps both, and settles
                // the Turn in the same breath when it arrives already settled.
                started_at: None,
                settled_at: None,
                last_output_at: None,
                usage: None,
                cost: None,
                cost_basis: None,
                cost_details: None,
            },
        },
        SessionChange::MessageAdded {
            message: Message {
                id: MessageId::new(),
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: prompt.text.clone(),
                skill_invocations: prompt.skill_invocations.clone(),
                attachments: prompt.attachments.clone(),
                truncated: false,
            },
        },
    ];
    (
        DeliveredTurn {
            prompt,
            turn_id,
            agent,
        },
        changes,
    )
}

fn snapshot_for_owner(state: &SessionStoreState, owner: &PromptOwner) -> SessionSnapshot {
    state
        .sessions
        .get(&owner.session_id)
        .expect("Prompt owner always references its Session")
        .snapshot
        .clone()
}

impl PromptOwner {
    fn canonical_creation_directory(&self, request: &CreateSessionRequest) -> Option<PathBuf> {
        if self.text != request.prompt.text
            || self.skill_invocations != request.prompt.skill_invocations
            || self.attachments != request.prompt.attachments
            || self.agent_selection != request.agent_selection
        {
            return None;
        }
        match &self.origin {
            PromptOrigin::SessionCreation {
                canonical_execution_directory,
                ..
            } => Some(canonical_execution_directory.clone()),
            PromptOrigin::Admission(_) => None,
        }
    }

    fn matches_canonical_creation(&self, request: &CreateSessionRequest, workspace: &Path) -> bool {
        self.canonical_creation_directory(request)
            .is_some_and(|canonical| canonical == workspace)
    }

    fn matches_admission(&self, session_id: SessionId, request: &AdmitPromptRequest) -> bool {
        self.session_id == session_id
            && self.text == request.prompt.text
            && self.skill_invocations == request.prompt.skill_invocations
            && self.attachments == request.prompt.attachments
            && matches!(&self.origin, PromptOrigin::Admission(delivery) if *delivery == request.delivery)
    }
}
