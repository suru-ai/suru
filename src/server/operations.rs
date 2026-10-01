//! The acts on Sessions: beginning a Session, admitting a Prompt to one,
//! interrupting it, settling and unsettling it, and answering its
//! Questionnaire.
//!
//! Each act is one operation, decided here rather than in the route that
//! receives it, so anything that performs an act — a Client through the
//! Session API, or a Sidekick through the Broker — performs exactly the same
//! one. An operation needs no request to call: it takes the act's own terms,
//! brings the Session it acts on into memory as the Session API's hydration
//! boundary would, and answers a typed outcome or a typed refusal, which the
//! HTTP handlers and the Broker's Tools only shape into their answers. A
//! refusal says why in one sentence of its own, so a Client's reader and a
//! Sidekick are told the same thing.
//!
//! An act may name its author: who performs it on the user's behalf, where
//! the user does not perform it themselves. A Sidekick's act on a Session of
//! the Sidekick Workspace — its own included — is refused here, where every
//! act passes, so no Sidekick sets another to work however it asks (ADR
//! 0043); reading such a Session is no act and is never refused.

use std::{path::Path, sync::Arc, time::Duration};

use tokio::sync::watch;

use super::LandingAgentSelectionStore;
use crate::attachments::{AttachmentStore, BindingRefusal, PromptAttachmentError};
use crate::model_catalog::ModelCatalogService;
use crate::protocol::{
    AdmitPromptRequest, AgentSelection, AttachmentDescriptor, Author, CreateSessionRequest,
    InitialPrompt, InterruptOutcome, Prompt, PromptId, ProviderId, QuestionnaireId,
    QuestionnaireSubmission, SessionId, SessionSnapshot, SessionSummary, SettingsSnapshot,
    SkillCatalogRequest, SkillPromptDelivery,
};
use crate::provider::ProviderOrchestrator;
use crate::sessions::{
    AdmitPromptError, ApprovalPostureUpdate, CreateSessionError, Derivation, InterruptSessionError,
    PromptAdmissionDisposition, SessionStore, SettleSessionError, StoreOutcome,
};
use crate::sidekick::SidekickWorkspace;
use crate::skill_catalog::{SkillCatalogError, SkillCatalogService};
use crate::source_control::{PreparationStore, SourceControlService};
use crate::storage::StorageError;

/// What a Sidekick is told, and a Client's reader would be, of an act it sent
/// to a Session of the Sidekick Workspace.
const SIDEKICK_WORKSPACE_REFUSAL: &str = "The Session is one of the Sidekick Workspace's, and no \
     Sidekick acts on a Session there, its own included, though it may read one.";

/// What every refusal says of a Session this Server does not hold.
const SESSION_NOT_FOUND: &str = "The Session does not exist on this Suru server.";

/// What every refusal says when the Server's own storage failed it.
const STORAGE_FAILED: &str = "Suru's own storage failed, so nothing was done; its Log says how.";

/// Why a Prompt was refused, whether it was to begin a Session or to be
/// admitted to one. Beginning a Session is refused with
/// [`Self::AgentSelection`] or [`Self::RepositoryLabels`] and never with
/// [`Self::SessionNotFound`] or [`Self::SubagentSession`]; admitting a Prompt
/// the other way round.
#[derive(Debug)]
pub(crate) enum PromptRefusal {
    /// The Session the Prompt was sent to does not exist on this Server.
    SessionNotFound,
    /// The Session the Prompt was sent to is a Subagent's, which is offered
    /// Delegations rather than Prompts.
    SubagentSession,
    /// The Prompt has no text but whitespace.
    EmptyPrompt,
    /// The Prompt's identity is already another Prompt's, or the same
    /// Prompt's with other content or admission metadata.
    PromptConflict,
    /// The Prompt's Attachment bindings cannot stand.
    Attachment(BindingRefusal),
    /// A Skill the Prompt invokes cannot be invoked where it would run.
    Skill(SkillCatalogError),
    /// The Agent Selection a Session was asked to begin with cannot be run
    /// here.
    AgentSelection(AgentSelectionRefusal),
    /// The Execution Directory, its Worktree, or the Worktree's preparation
    /// cannot take the Prompt now, for the reason given.
    InvalidWorkspace(String),
    /// The presented roots of this Server's Workspaces could not be brought up
    /// to date before the Session was recorded among them.
    RepositoryLabels(String),
    /// A Sidekick sent the Prompt to a Session of the Sidekick Workspace.
    SidekickWorkspace,
    /// The Server's own storage failed it; the Log says how.
    Storage,
}

impl std::fmt::Display for PromptRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNotFound => formatter.write_str(SESSION_NOT_FOUND),
            Self::SubagentSession => formatter.write_str(
                "A Subagent's Session refuses Prompts; it is sent Delegations by the Agent that \
                 delegated to it.",
            ),
            Self::EmptyPrompt => {
                formatter.write_str("A Prompt must contain text other than whitespace.")
            }
            Self::PromptConflict => formatter.write_str(
                "The Prompt's identity is already another Prompt's, or the same Prompt's with \
                 other content or admission metadata.",
            ),
            Self::Attachment(refusal) => formatter.write_str(&refusal.message()),
            Self::Skill(error) => write!(formatter, "{error}"),
            Self::AgentSelection(refusal) => write!(formatter, "{refusal}"),
            Self::InvalidWorkspace(reason) | Self::RepositoryLabels(reason) => {
                formatter.write_str(reason)
            }
            Self::SidekickWorkspace => formatter.write_str(SIDEKICK_WORKSPACE_REFUSAL),
            Self::Storage => formatter.write_str(STORAGE_FAILED),
        }
    }
}

/// Why an Agent Selection cannot be run on this Server.
#[derive(Debug)]
pub(crate) enum AgentSelectionRefusal {
    /// It names a Provider this Server does not host.
    ProviderNotHosted,
    /// Its Model or Model Options are not ones its Provider offers, for the
    /// reason given.
    Invalid(String),
}

impl std::fmt::Display for AgentSelectionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderNotHosted => formatter
                .write_str("The Agent Selection names a Provider this Suru server does not host."),
            Self::Invalid(reason) => write!(formatter, "Agent Selection is invalid: {reason}"),
        }
    }
}

/// Why interrupting a Session was refused.
#[derive(Debug)]
pub(crate) enum InterruptRefusal {
    /// The Session does not exist on this Server.
    SessionNotFound,
    /// Nothing below the Session is running: no active Turn, no working
    /// Subagent, and no live Watch anywhere in its subtree.
    NothingToInterrupt,
    /// The interrupt named a Subagent's Session whose Provider offers no
    /// per-Subagent stop.
    SubagentStopUnsupported,
    /// The Provider did not take the interrupt, for the reason given.
    ProviderFailure(String),
    /// A Sidekick sent the interrupt to a Session of the Sidekick Workspace.
    SidekickWorkspace,
    /// The Server's own storage failed it; the Log says how.
    Storage,
}

impl std::fmt::Display for InterruptRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionNotFound => formatter.write_str(SESSION_NOT_FOUND),
            Self::NothingToInterrupt => formatter.write_str(
                "The Session has no active Turn, no working Subagent, and no live Watch, so \
                 there is nothing to interrupt.",
            ),
            Self::SubagentStopUnsupported => formatter.write_str(
                "The Subagent's Provider offers no per-Subagent stop, so it cannot be stopped on \
                 its own.",
            ),
            Self::ProviderFailure(reason) => {
                write!(
                    formatter,
                    "The Provider did not take the interrupt: {reason}"
                )
            }
            Self::SidekickWorkspace => formatter.write_str(SIDEKICK_WORKSPACE_REFUSAL),
            Self::Storage => formatter.write_str(STORAGE_FAILED),
        }
    }
}

/// Why settling or unsettling a Session was refused.
#[derive(Debug)]
pub(crate) enum SettleRefusal {
    /// The Session does not exist on this Server.
    SessionNotFound,
    /// A Sidekick sent the act to a Session of the Sidekick Workspace.
    SidekickWorkspace,
    /// The Server's own storage failed it; the Log says how.
    Storage,
}

impl std::fmt::Display for SettleRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SessionNotFound => SESSION_NOT_FOUND,
            Self::SidekickWorkspace => SIDEKICK_WORKSPACE_REFUSAL,
            Self::Storage => STORAGE_FAILED,
        })
    }
}

/// Why answering a Questionnaire was refused.
#[derive(Debug)]
pub(crate) enum AnswerRefusal {
    /// The Answer did not reach the Questionnaire, or its delivery could not
    /// be confirmed, for the reason given.
    SubmissionFailed(String),
    /// The Server's own storage failed it; the Log says how.
    Storage,
}

/// A Prompt admitted to a Session, and how it was admitted: `None` for a
/// retry that found it already admitted, which admits nothing again.
#[derive(Debug)]
pub(crate) struct AdmittedPrompt {
    pub(crate) prompt: Prompt,
    pub(crate) delivery: Option<AdmittedDelivery>,
}

/// How a Prompt was admitted, which the Session's own state decided as much as
/// the delivery it was sent with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdmittedDelivery {
    /// The Session was running no Turn it could take it into, so it begins a
    /// Turn of its own.
    NewTurn,
    /// It steers the Turn the Session is working in.
    Steer,
    /// It waits behind the Turn the Session is working in, to begin the next.
    Queued,
}

/// The Server-side services the acts on Sessions are performed with.
#[derive(Clone)]
pub(crate) struct SessionOperations {
    sessions: SessionStore,
    providers: ProviderOrchestrator,
    source_control: SourceControlService,
    preparations: PreparationStore,
    skill_catalog: SkillCatalogService,
    model_catalog: ModelCatalogService,
    attachments: AttachmentStore,
    landing_agent_selection: LandingAgentSelectionStore,
    /// Derives a Session's Title, its Workspace's Icon where it has none, and
    /// a better name for the branch of a Managed Worktree just prepared for
    /// it, from its first Prompt, in the background and beside the first Turn
    /// rather than in front of it.
    derivation: Derivation,
    settings: watch::Receiver<SettingsSnapshot>,
    /// The Providers this server hosts, in the fixed built-in order. Agent
    /// Selections normalize against this set rather than any single Provider
    /// identity.
    hosted_providers: Arc<Vec<ProviderId>>,
    checkout_skill_timeout: Duration,
    /// Whose Sessions no Sidekick acts on.
    sidekick_workspace: SidekickWorkspace,
}

impl SessionOperations {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        sessions: SessionStore,
        providers: ProviderOrchestrator,
        source_control: SourceControlService,
        preparations: PreparationStore,
        skill_catalog: SkillCatalogService,
        model_catalog: ModelCatalogService,
        attachments: AttachmentStore,
        landing_agent_selection: LandingAgentSelectionStore,
        derivation: Derivation,
        settings: watch::Receiver<SettingsSnapshot>,
        hosted_providers: Arc<Vec<ProviderId>>,
        checkout_skill_timeout: Duration,
        sidekick_workspace: SidekickWorkspace,
    ) -> Self {
        Self {
            sessions,
            providers,
            source_control,
            preparations,
            skill_catalog,
            model_catalog,
            attachments,
            landing_agent_selection,
            derivation,
            settings,
            hosted_providers,
            checkout_skill_timeout,
            sidekick_workspace,
        }
    }

    /// Begins a Session with its first Prompt, answering the Session made, or
    /// the one an earlier attempt at the same beginning already made.
    ///
    /// A beginning named by a Worktree preparation joins that preparation:
    /// it rejoins a Session the preparation already admitted, and otherwise
    /// begins in the Worktree it prepared, holding the Repository's checkout
    /// lease through to the first Turn. A Session newly made has its first
    /// Turn scheduled, and only then its Title derived.
    pub(crate) async fn begin_session(
        &self,
        mut request: CreateSessionRequest,
    ) -> Result<StoreOutcome<SessionSnapshot>, PromptRefusal> {
        self.check_prompt_attachments(&request.prompt).await?;

        let _preparation_serial = if request.preparation_id.is_some() {
            Some(self.preparations.serial.lock().await)
        } else {
            None
        };
        if request.preparation_id.is_some() {
            self.hydrate_prompt_owner(request.prompt.id).await?;
        }
        let mut missing_preparation = false;
        let mut preparation = match request.preparation_id {
            Some(id) => match self.preparations.load(id) {
                Ok(Some(plan)) => Some(plan),
                Ok(None) => {
                    missing_preparation = true;
                    None
                }
                Err(e) => return Err(invalid_workspace(e)),
            },
            None => None,
        };
        if let Some(plan) = &mut preparation {
            if request.execution_directory != plan.destination {
                return Err(invalid_workspace(
                    "Preparation identity belongs to another execution location",
                ));
            }
            if let Some(snapshot) = self
                .rejoin_preparation(plan)
                .await
                .map_err(invalid_workspace)?
            {
                return Ok(StoreOutcome::Existing(snapshot));
            }
        }
        let mut mutation = if let Some(plan) = &preparation {
            Some(
                self.source_control
                    .mutation_guard(&plan.repository.id)
                    .await,
            )
        } else {
            None
        };
        if let Some(plan) = &preparation {
            if !plan.ready || request.execution_directory != plan.destination {
                return Err(invalid_workspace(
                    "Prepare the intended Worktree before admitting this Prompt",
                ));
            }
            self.source_control
                .prepare_checkout(plan)
                .await
                .map_err(invalid_workspace)?;
        }

        self.hydrate_prompt_owner(request.prompt.id).await?;

        if let Some(selection) = request.agent_selection.take() {
            request.agent_selection = Some(
                self.normalize_agent_selection(selection)
                    .map_err(PromptRefusal::AgentSelection)?,
            );
        } else {
            // A persisted Landing selection can predate this server's hosted set or
            // the user's own choice of Providers, so one naming a Provider this
            // server does not host — or one the user has since turned off — yields
            // to the built-in default rather than stranding them on it.
            request.agent_selection = self
                .landing_agent_selection
                .current()
                .filter(|selection| self.is_selectable_provider(&selection.provider))
                .or_else(|| self.model_catalog.default_selection());
        }

        let mut location = self
            .source_control
            .resolve(&request.execution_directory.path, None)
            .await;
        if mutation.is_none()
            && !missing_preparation
            && let Some(repository) = &location.workspace.repository
        {
            mutation = Some(self.source_control.mutation_guard(&repository.id).await);
            let current = self
                .source_control
                .resolve(&request.execution_directory.path, Some(&location.workspace))
                .await;
            if current.checkout.as_ref().map(|c| &c.id) != location.checkout.as_ref().map(|c| &c.id)
                || current.execution_status != crate::protocol::ExecutionDirectoryStatus::Available
            {
                return Err(invalid_workspace(
                    "Execution location changed before admission; choose or restore the Worktree and retry",
                ));
            }
            location = current;
        }
        if location.execution_directory.is_none() {
            return Err(invalid_workspace(
                "Choose a working copy before starting a Session; repository metadata is not an Execution Directory",
            ));
        }
        self.sessions
            .refresh_repository_labels(&self.source_control)
            .map_err(|error| PromptRefusal::RepositoryLabels(error.to_string()))?;
        let provider = self.skill_catalog_provider(request.agent_selection.as_ref());
        if request.preparation_id.is_some()
            && !request.prompt.skill_invocations.is_empty()
            && let Some(provider) = provider.clone()
        {
            request.prompt = self
                .skill_catalog
                .rebind_prepared_prompt(
                    provider,
                    &request.execution_directory.path,
                    &request.prompt,
                )
                .await
                .map_err(|error| {
                    PromptRefusal::Skill(SkillCatalogError::InvalidInvocation(format!(
                        "Destination Skills must match before Prompt admission: {error:?}"
                    )))
                })?;
        }
        self.validate_new_prompt_skills(
            provider,
            &request.execution_directory.path,
            &request.prompt,
            SkillPromptDelivery::Initial,
        )
        .await?;

        if missing_preparation {
            return match self.sessions.existing_prepared_creation(&request) {
                Ok(Some(snapshot)) => Ok(StoreOutcome::Existing(snapshot)),
                Ok(None) => Err(invalid_workspace(
                    "Worktree preparation is unknown; prepare it before admission",
                )),
                Err(CreateSessionError::PromptConflict) => Err(PromptRefusal::PromptConflict),
                Err(_) => unreachable!("existing creation only reports Prompt conflicts"),
            };
        }

        // Checked again after every await above: the first check refuses a bad
        // binding before any checkout work and reads only, while this one also
        // stamps every bound Attachment as referenced now, which keeps a Session's
        // deletion from reclaiming it before the flush joins it (ADR 0037).
        let described = self.reference_prompt_attachments(&request.prompt).await?;

        let admission = if let Some(plan) = &preparation {
            self.sessions.create_in_with_identity(
                request,
                location,
                described,
                Some(plan.intended_session),
            )
        } else {
            self.sessions.create_in(request, location, described)
        };
        let mut snapshot = match admission {
            Ok(StoreOutcome::Created(snapshot)) => snapshot,
            Ok(StoreOutcome::Existing(snapshot)) => return Ok(StoreOutcome::Existing(snapshot)),
            Err(CreateSessionError::EmptyPrompt) => return Err(PromptRefusal::EmptyPrompt),
            Err(CreateSessionError::InvalidWorkspace) => {
                return Err(invalid_workspace(
                    "Workspace must be an existing local directory",
                ));
            }
            Err(CreateSessionError::PromptConflict) => return Err(PromptRefusal::PromptConflict),
        };
        self.settle_actorless_posture(self.sessions.reconcile_tree_approval_posture(
            snapshot.session.id,
            &self.settings.borrow().settings,
        ));
        snapshot = self
            .sessions
            .snapshot(snapshot.session.id)
            .unwrap_or(snapshot);
        if let Some(plan) = &mut preparation {
            self.sessions
                .persist_prepared_session(snapshot.session.id)
                .map_err(|error| invalid_workspace(error.to_string()))?;
            self.source_control
                .checkpoint(
                    crate::source_control::PreparationCheckpoint::SessionPersisted,
                    plan,
                )
                .await
                .map_err(invalid_workspace)?;
            plan.admitted_session = Some(snapshot.session.id);
            if let Err(e) = self.preparations.delete_after_admission(plan) {
                tracing::warn!("Admitted Worktree preparation intent could not be deleted: {e}");
            }
        }

        if let Some(selection) = snapshot.session.agent_selection.clone() {
            self.landing_agent_selection.confirm(selection);
        }
        if let Some(guard) = mutation.take() {
            self.providers
                .hold_checkout_guard(snapshot.session.id, snapshot.prompts[0].id, guard);
        }
        self.providers.open_session(
            snapshot.session.id,
            snapshot.session.execution_directory.path.clone(),
            snapshot.prompts[0].id,
        );
        if let Some(plan) = &preparation {
            self.source_control
                .checkpoint(crate::source_control::PreparationCheckpoint::Admitted, plan)
                .await
                .map_err(invalid_workspace)?;
        }
        // After the Turn is scheduled and never in front of it: a Title is
        // cosmetic and the user's actual work does not wait on one. Only a
        // freshly created Session reaches here, which is what makes the
        // derivation once-per-Session — a retried creation answers with the
        // Session it already made and asks for nothing. The same holds for
        // the branch a fresh preparation just created, the one thing that
        // lets derivation propose renaming it.
        let created_branch = preparation.as_ref().and_then(|plan| {
            let checkout = snapshot
                .session
                .checkout
                .clone()
                .filter(|checkout| checkout.root == plan.destination.path)?;
            Some(crate::source_control::CreatedBranch {
                repository: plan.repository.clone(),
                checkout,
                branch: plan.plan.branch()?.to_owned(),
            })
        });
        self.derivation.derive(
            snapshot.session.id,
            snapshot.session.execution_directory.path.clone(),
            snapshot
                .session
                .agent_selection
                .as_ref()
                .map(|selection| selection.provider.clone()),
            &snapshot.prompts[0],
            &snapshot.session.workspace,
            created_branch,
        );
        Ok(StoreOutcome::Created(snapshot))
    }

    /// Admits a Prompt to a Session, answering the Prompt admitted, or the one
    /// an earlier attempt at the same admission already admitted, and how it
    /// was admitted. `author` names who sent it on the user's behalf, where
    /// the user did not, and the Prompt and the Message it becomes carry it.
    ///
    /// A Prompt new to a Session working in a Worktree first takes that
    /// Worktree's checkout lease, recovering the Worktree where it has gone.
    /// One admitted to begin a Turn is scheduled with the lease held through
    /// to it, and one admitted to steer a working Turn is steered in.
    pub(crate) async fn admit_prompt(
        &self,
        session_id: SessionId,
        request: AdmitPromptRequest,
        author: Option<Author>,
    ) -> Result<StoreOutcome<AdmittedPrompt>, PromptRefusal> {
        self.hydrate(session_id)
            .await
            .map_err(|_| PromptRefusal::Storage)?;
        if self.refuses_author(session_id, author.as_ref()) {
            return Err(PromptRefusal::SidekickWorkspace);
        }
        self.check_prompt_attachments(&request.prompt).await?;

        self.hydrate_prompt_owner(request.prompt.id).await?;

        let mut execution = None;
        if !self.sessions.knows_prompt(request.prompt.id)
            && let Some(snapshot) = self.sessions.snapshot(session_id)
            && snapshot.session.checkout.is_some()
        {
            let lease = self
                .source_control
                .prepare_execution(&snapshot.session, None)
                .await
                .map_err(|e| {
                    invalid_workspace(format!(
                        "Worktree unavailable; retry after resolving recovery: {e}"
                    ))
                })?;
            if let Some(reading) = lease.reading.clone()
                && let Err(error) = self.sessions.record_execution_checkout(reading)
            {
                return Err(invalid_workspace(format!(
                    "Cannot persist current checkout recovery facts: {error}"
                )));
            }
            if snapshot
                .session
                .checkout
                .as_ref()
                .is_some_and(|c| c.kind == crate::protocol::CheckoutKind::Linked)
            {
                let provider =
                    self.skill_catalog_provider(snapshot.session.agent_selection.as_ref());
                if let Some(provider) = provider {
                    match tokio::time::timeout(
                        self.checkout_skill_timeout,
                        self.skill_catalog.refresh_current(SkillCatalogRequest {
                            provider,
                            execution_directory: snapshot.session.execution_directory.clone(),
                        }),
                    )
                    .await
                    {
                        Ok(Ok(catalog))
                            if matches!(
                                catalog.status,
                                crate::protocol::SkillCatalogStatus::Fresh { .. }
                            ) => {}
                        _ => {
                            return Err(invalid_workspace(
                                "Destination Skills are unavailable; Worktree retained, retry after restoring the catalog",
                            ));
                        }
                    }
                }
            }
            execution = Some(lease);
        }

        if !request.prompt.skill_invocations.is_empty()
            && !self.sessions.knows_prompt(request.prompt.id)
        {
            let Some(snapshot) = self.sessions.snapshot(session_id) else {
                return Err(PromptRefusal::SessionNotFound);
            };
            let provider = self.skill_catalog_provider(snapshot.session.agent_selection.as_ref());
            // The client's Enter always asks to steer, but an idle Session starts
            // the Prompt as a Turn of its own; judge the delivery it will get.
            let delivery = match crate::sessions::effective_delivery(&snapshot, request.delivery) {
                crate::protocol::PromptDelivery::Queue => SkillPromptDelivery::Queue,
                crate::protocol::PromptDelivery::Steer => SkillPromptDelivery::Steer,
            };
            self.validate_new_prompt_skills(
                provider,
                &snapshot.session.execution_directory.path,
                &request.prompt,
                delivery,
            )
            .await?;
        }

        // Checked again after every await above: the first check refuses a bad
        // binding before any checkout work and reads only, while this one also
        // stamps every bound Attachment as referenced now, which keeps a Session's
        // deletion from reclaiming it before the flush joins it (ADR 0037).
        let described = self.reference_prompt_attachments(&request.prompt).await?;

        // Recovery and catalog refresh await external work. Admission must use the
        // current Turn state, and the actor repeats this check at native steering.
        if let Some(lease) = &execution
            && let Some(current) = self.sessions.snapshot(session_id)
            && crate::sessions::effective_delivery(&current, request.delivery)
                == crate::protocol::PromptDelivery::Steer
            && current.session.working_since.is_some()
            && self
                .providers
                .connected_incarnation(session_id)
                .is_some_and(|incarnation| incarnation != lease.incarnation)
        {
            return Err(invalid_workspace(
                "The Worktree was recreated while this Agent is still Working; wait for it to settle before retrying",
            ));
        }

        match self.sessions.admit(session_id, request, described, author) {
            Ok(StoreOutcome::Created(admission)) => {
                let admitted = AdmittedPrompt {
                    delivery: Some(match admission.disposition {
                        PromptAdmissionDisposition::StartImmediately => AdmittedDelivery::NewTurn,
                        PromptAdmissionDisposition::SteerActive => AdmittedDelivery::Steer,
                        PromptAdmissionDisposition::RemainPending => AdmittedDelivery::Queued,
                    }),
                    prompt: admission.prompt,
                };
                match admission.disposition {
                    PromptAdmissionDisposition::StartImmediately => {
                        if let Some(guard) = execution.as_mut().and_then(|lease| lease.guard.take())
                        {
                            self.providers.hold_checkout_guard(
                                session_id,
                                admitted.prompt.id,
                                guard,
                            );
                        }
                        self.providers
                            .schedule_prompt(session_id, admitted.prompt.id)
                            .expect("stored Sessions retain their Provider actor");
                    }
                    PromptAdmissionDisposition::SteerActive => self
                        .providers
                        .schedule_steer(session_id)
                        .expect("stored Sessions retain their Provider actor"),
                    PromptAdmissionDisposition::RemainPending => {}
                }
                Ok(StoreOutcome::Created(admitted))
            }
            Ok(StoreOutcome::Existing(admission)) => Ok(StoreOutcome::Existing(AdmittedPrompt {
                prompt: admission.prompt,
                delivery: None,
            })),
            Err(AdmitPromptError::EmptyPrompt) => Err(PromptRefusal::EmptyPrompt),
            Err(AdmitPromptError::SessionNotFound) => Err(PromptRefusal::SessionNotFound),
            Err(AdmitPromptError::SubagentSession) => Err(PromptRefusal::SubagentSession),
            Err(AdmitPromptError::PromptConflict) => Err(PromptRefusal::PromptConflict),
        }
    }

    /// Interrupts a Session for `author`, answering whether it stopped work or
    /// withdrew a Prompt not yet delivered (ADR 0024).
    pub(crate) async fn interrupt_session(
        &self,
        session_id: SessionId,
        author: Option<&Author>,
    ) -> Result<InterruptOutcome, InterruptRefusal> {
        self.hydrate(session_id)
            .await
            .map_err(|_| InterruptRefusal::Storage)?;
        if self.refuses_author(session_id, author) {
            return Err(InterruptRefusal::SidekickWorkspace);
        }
        self.providers
            .interrupt_session(session_id)
            .await
            .map_err(|error| match error {
                InterruptSessionError::SessionNotFound => InterruptRefusal::SessionNotFound,
                InterruptSessionError::NothingToInterrupt => InterruptRefusal::NothingToInterrupt,
                InterruptSessionError::SubagentStopUnsupported => {
                    InterruptRefusal::SubagentStopUnsupported
                }
                InterruptSessionError::ProviderFailure(reason) => {
                    InterruptRefusal::ProviderFailure(reason)
                }
                InterruptSessionError::Storage(error) => {
                    tracing::warn!("an interrupt could not withdraw its Prompt: {error}");
                    InterruptRefusal::Storage
                }
            })
    }

    /// Sets a Session aside as done for now, or brings it back, for `author`,
    /// answering the summary the change left standing.
    pub(crate) async fn settle_session(
        &self,
        session_id: SessionId,
        settled: bool,
        author: Option<&Author>,
    ) -> Result<SessionSummary, SettleRefusal> {
        self.hydrate(session_id)
            .await
            .map_err(|_| SettleRefusal::Storage)?;
        if self.refuses_author(session_id, author) {
            return Err(SettleRefusal::SidekickWorkspace);
        }
        self.sessions
            .settle(session_id, settled)
            .map_err(|SettleSessionError::SessionNotFound| SettleRefusal::SessionNotFound)
    }

    /// Answers a Session's Questionnaire, once its Provider has taken the
    /// Answer.
    pub(crate) async fn answer_questionnaire(
        &self,
        session_id: SessionId,
        id: QuestionnaireId,
        submission: QuestionnaireSubmission,
    ) -> Result<(), AnswerRefusal> {
        self.hydrate(session_id)
            .await
            .map_err(|_| AnswerRefusal::Storage)?;
        self.providers
            .submit_questionnaire(session_id, id, submission)
            .await
            .map_err(AnswerRefusal::SubmissionFailed)
    }

    /// An Agent Selection as this Server runs it, refused where it names a
    /// Provider this Server does not host or a Model its Provider does not
    /// offer.
    pub(super) fn normalize_agent_selection(
        &self,
        selection: AgentSelection,
    ) -> Result<AgentSelection, AgentSelectionRefusal> {
        if !self.hosted_providers.contains(&selection.provider) {
            return Err(AgentSelectionRefusal::ProviderNotHosted);
        }
        self.model_catalog
            .normalize_selection(&selection)
            .map_err(AgentSelectionRefusal::Invalid)
    }

    /// A posture owed to a Provider actor that does not exist is applied by
    /// nobody, so it is recorded as applied at once: the actor that starts next
    /// starts under it, and no reader waits on a delivery that will never come.
    pub(super) fn settle_actorless_posture(&self, update: Option<ApprovalPostureUpdate>) {
        if let Some(update) = update
            && !self.providers.has_session_actor(update.session_id)
        {
            self.sessions.mark_approval_posture_application(
                update,
                crate::protocol::ApprovalPostureApplication::Applied,
            );
        }
    }

    /// An admitted initial Prompt is immutable even if a Client edits its retry.
    /// Hydrate only this preparation's intended Session, never the catalog.
    pub(super) async fn rejoin_preparation(
        &self,
        plan: &mut crate::protocol::PreparedCheckout,
    ) -> Result<Option<SessionSnapshot>, String> {
        self.sessions
            .hydrate(plan.intended_session)
            .await
            .map_err(|e| e.to_string())?;
        let Some(snapshot) = self.sessions.snapshot(plan.intended_session) else {
            return if plan.admitted_session.is_some() {
                Err("The admitted Session no longer exists".into())
            } else {
                Ok(None)
            };
        };
        if snapshot.session.execution_directory != plan.destination {
            return Err("Preparation Session has a conflicting execution location".into());
        }
        let pending = snapshot
            .turns
            .is_empty()
            .then(|| {
                snapshot
                    .prompts
                    .iter()
                    .find(|prompt| prompt.status == crate::protocol::PromptStatus::Pending)
            })
            .flatten();
        if let Some(prompt) = pending
            && !self.providers.has_session_actor(snapshot.session.id)
        {
            let guard = self
                .source_control
                .mutation_guard(&plan.repository.id)
                .await;
            self.source_control.prepare_checkout(plan).await?;
            let provider = self.skill_catalog_provider(snapshot.session.agent_selection.as_ref());
            if let Some(provider) = &provider {
                let catalog = tokio::time::timeout(
                    self.checkout_skill_timeout,
                    self.skill_catalog.refresh_current(SkillCatalogRequest {
                        provider: provider.clone(),
                        execution_directory: plan.destination.clone(),
                    }),
                )
                .await
                .map_err(|_| "Destination Skill discovery timed out; retry".to_owned())?
                .map_err(|error| format!("Destination Skills are unavailable; retry: {error:?}"))?;
                if !matches!(
                    catalog.status,
                    crate::protocol::SkillCatalogStatus::Fresh { .. }
                ) {
                    return Err(
                        "Destination Skills are unavailable; Worktree retained for retry".into(),
                    );
                }
            }
            let initial = InitialPrompt {
                id: prompt.id,
                text: prompt.text.clone(),
                skill_invocations: prompt.skill_invocations.clone(),
                attachments: prompt.attachments.clone(),
            };
            if !initial.skill_invocations.is_empty() {
                let provider =
                    provider.ok_or("No Provider is selected for the admitted Skill Invocation")?;
                // This Prompt is already known, but it has never started. Admission's
                // idempotency bypass must not skip destination validation here.
                self.skill_catalog.validate_prompt(provider, &plan.destination.path, &initial, SkillPromptDelivery::Initial)
                    .await.map_err(|error| format!("The admitted Prompt's destination Skills must be available before startup: {error:?}"))?;
            }
            self.sessions
                .persist_prepared_session(snapshot.session.id)
                .map_err(|e| e.to_string())?;
            self.providers
                .hold_checkout_guard(snapshot.session.id, prompt.id, guard);
            self.providers.open_session(
                snapshot.session.id,
                plan.destination.path.clone(),
                prompt.id,
            );
        }
        plan.admitted_session = Some(snapshot.session.id);
        self.preparations.delete_after_admission(plan)?;
        Ok(Some(snapshot))
    }

    /// Whether a remembered Agent Selection may still be handed to a new
    /// Session: its Provider is one this server hosts, and one the user has
    /// left enabled. Availability is deliberately not asked here — a Provider
    /// the user can fix from outside Suru keeps the selection they made, and
    /// the fresh-Landing default behind this is what passes over one that
    /// cannot work.
    fn is_selectable_provider(&self, provider: &ProviderId) -> bool {
        self.hosted_providers.contains(provider)
            && self.settings.borrow().settings.provider_enabled(provider)
    }

    /// The Provider whose Skill Catalog a Prompt's Skills are judged against:
    /// the one `selection` names, or the first this Server hosts where it
    /// names none.
    fn skill_catalog_provider(&self, selection: Option<&AgentSelection>) -> Option<ProviderId> {
        selection
            .map(|selection| selection.provider.clone())
            .or_else(|| self.hosted_providers.first().cloned())
    }

    /// Whether an act `author` performs on `session_id` is refused for its
    /// author: a Sidekick's act on a Session of the Sidekick Workspace, its
    /// own included. The user's own act never is, and nor is any act on a
    /// Session this Server does not hold, which is refused for that instead.
    fn refuses_author(&self, session_id: SessionId, author: Option<&Author>) -> bool {
        match author {
            None => false,
            Some(Author::Sidekick { .. }) => self
                .sessions
                .session(session_id)
                .is_some_and(|session| self.sidekick_workspace.holds(&session.workspace)),
        }
    }

    /// Brings the tree of the Session an act names into memory, as the Session
    /// API's hydration boundary does for every request naming one.
    async fn hydrate(&self, session_id: SessionId) -> Result<(), StorageError> {
        self.sessions
            .hydrate(session_id)
            .await
            .inspect_err(|error| tracing::warn!("Session hydration failed: {error}"))
    }

    async fn hydrate_prompt_owner(&self, prompt_id: PromptId) -> Result<(), PromptRefusal> {
        self.sessions
            .hydrate_prompt_owner(prompt_id)
            .await
            .map_err(|error| {
                tracing::warn!("Prompt owner hydration failed: {error}");
                PromptRefusal::Storage
            })
    }

    /// Refuses a Prompt whose Attachment bindings cannot stand: too many of
    /// them, a label its text does not carry where it is bound, or an
    /// Attachment this Server has not stored. Reads only, so it may run before
    /// any other work.
    async fn check_prompt_attachments(&self, prompt: &InitialPrompt) -> Result<(), PromptRefusal> {
        self.attachments
            .check_prompt(&prompt.text, &prompt.attachments)
            .await
            .map_err(attachment_refusal)
    }

    /// Refuses a Prompt as [`Self::check_prompt_attachments`] does, and
    /// otherwise stamps every Attachment it binds as referenced now, answering
    /// the descriptors the Session records beside the Prompt. Runs right before
    /// the Prompt is recorded, after every await of its admission, so no
    /// Session's deletion can reclaim a bound Attachment before the flush that
    /// joins it.
    async fn reference_prompt_attachments(
        &self,
        prompt: &InitialPrompt,
    ) -> Result<Vec<AttachmentDescriptor>, PromptRefusal> {
        self.attachments
            .reference_prompt(&prompt.text, &prompt.attachments)
            .await
            .map_err(attachment_refusal)
    }

    async fn validate_new_prompt_skills(
        &self,
        provider: Option<ProviderId>,
        execution_directory: &Path,
        prompt: &InitialPrompt,
        delivery: SkillPromptDelivery,
    ) -> Result<(), PromptRefusal> {
        if prompt.skill_invocations.is_empty() || self.sessions.knows_prompt(prompt.id) {
            return Ok(());
        }
        let provider = provider.ok_or_else(|| {
            PromptRefusal::Skill(SkillCatalogError::InvalidInvocation(
                "No Provider is selected for this Skill Invocation".to_owned(),
            ))
        })?;
        self.skill_catalog
            .validate_prompt(provider, execution_directory, prompt, delivery)
            .await
            .map_err(PromptRefusal::Skill)
    }
}

fn invalid_workspace(reason: impl Into<String>) -> PromptRefusal {
    PromptRefusal::InvalidWorkspace(reason.into())
}

fn attachment_refusal(error: PromptAttachmentError) -> PromptRefusal {
    match error {
        PromptAttachmentError::Refused(refusal) => PromptRefusal::Attachment(refusal),
        PromptAttachmentError::Storage(error) => {
            tracing::warn!("Prompt Attachments could not be checked: {error}");
            PromptRefusal::Storage
        }
    }
}
