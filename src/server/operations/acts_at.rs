//! Acting at an Origin: every act a Sidekick performs on a Session or a
//! Workspace, on this Server or on a Remote, through one interface.
//!
//! An act at this Server is the very operation a Client's request performs.
//! An act at a Remote is carried there through the Pairing as a Client's act
//! on that Remote is — the request a Client turned toward it would send, to
//! the Remote's own Session API — with the Sidekick named as its author, and
//! the Remote decides it as it decides every act: it refuses a Sidekick's act
//! on its own Sidekick Workspace, and stands what it takes as a Sidekick's on
//! this Peer (ADR 0044). Only acts on Sessions and Workspaces cross: nothing
//! here reaches a Remote's Settings, which a Remote refuses a Peer, nor
//! anything else of its administration.
//!
//! Nothing is kept to try again. An act at a Remote that cannot be asked, or
//! does not answer, is refused saying so — and saying whether it may have
//! been done there all the same — and the Sidekick asks again once the Remote
//! answers. Where it may have been done, nothing is recorded of it here as
//! though it had been; a Prompt it carried that a later read finds there
//! records it then (see [`super::uncertain`]).
//!
//! An act a Remote takes is recorded here, against the Sidekick's Session,
//! since only this Server knows both ends of it: the Session acted on — or
//! begun — there stands beneath the Sidekick's Session in its tree from then
//! on, as that Remote says of it (see [`super::remote_entries`]).

use axum::http::Method;

use super::{
    AdmittedDelivery, AnswerRefusal, InterruptRefusal, PreparationRefusal, PromptRefusal,
    SessionOperations, SettleRefusal,
    origins::{RemoteActRefusal, SESSIONS_PATH, WORKSPACES_PATH},
    uncertain::UncertainAct,
};
use crate::protocol::{
    AdmitPromptRequest, AgentSelection, Author, CreateSessionRequest, Health, InterruptOutcome,
    ModelCatalog, Outlook, PROMPT_ADMISSION_HEADER, PrepareCheckoutRequest, PrepareCheckoutResult,
    PromptId, QuestionnaireId, QuestionnaireSubmission, SessionId, SessionSnapshot, SessionSummary,
    SetWorkspaceDescriptionRequest, SettleSessionRequest, WorkspaceListing,
};
use crate::sessions::StoreOutcome;

/// Why an act at an Origin was refused: as this Server refuses it, in the
/// refusal `R` its own operation gives, or as a Remote did — or because the
/// Remote could not be asked.
#[derive(Debug)]
pub(crate) enum ActRefusal<R> {
    Here(R),
    There(RemoteActRefusal),
}

impl SessionOperations {
    /// Performs the act `act` at `origin`: as `here` performs it on this
    /// Server, refused in its own words, or as `there` carries it to the
    /// Remote `origin` names.
    async fn dispatch<A, T, R>(
        &self,
        origin: &Outlook,
        act: A,
        here: impl AsyncFnOnce(A) -> Result<T, R>,
        there: impl AsyncFnOnce(&str, A) -> Result<T, RemoteActRefusal>,
    ) -> Result<T, ActRefusal<R>> {
        match origin {
            Outlook::Local => here(act).await.map_err(ActRefusal::Here),
            Outlook::Remote(name) => there(name, act).await.map_err(ActRefusal::There),
        }
    }

    /// Begins a Session at `origin` for `author`, answering the Session
    /// begun, as [`Self::begin_session`] begins one here.
    pub(crate) async fn begin_session_at(
        &self,
        origin: &Outlook,
        request: CreateSessionRequest,
        author: Author,
    ) -> Result<SessionSnapshot, ActRefusal<PromptRefusal>> {
        self.dispatch(
            origin,
            request,
            async |request| match self.begin_session(request, Some(author.clone())).await? {
                StoreOutcome::Created(snapshot) | StoreOutcome::Existing(snapshot) => Ok(snapshot),
            },
            async |name, request| {
                let begun: SessionSnapshot = self
                    .remotes
                    .act(name, Method::POST, SESSIONS_PATH, Some(&request), &author)
                    .await
                    .and_then(|answered| answered.read())
                    .inspect_err(|refusal| {
                        self.note_uncertain(refusal, &author, name, request.prompt.id, true);
                    })?;
                self.record_remote_act(&author, name, begun.session.id, true)
                    .await;
                Ok(begun)
            },
        )
        .await
    }

    /// Prepares a new Managed Worktree at `origin` for a Session `author` is
    /// about to begin there, as [`Self::prepare_worktree`] prepares one here.
    pub(crate) async fn prepare_worktree_at(
        &self,
        origin: &Outlook,
        request: PrepareCheckoutRequest,
        author: Author,
    ) -> Result<PrepareCheckoutResult, ActRefusal<PreparationRefusal>> {
        self.dispatch(
            origin,
            request,
            async |request| self.prepare_worktree(request, Some(&author)).await,
            async |name, request| {
                self.remotes
                    .act(
                        name,
                        Method::POST,
                        "/v1/checkouts/prepare",
                        Some(&request),
                        &author,
                    )
                    .await
                    .and_then(|answered| answered.read())
            },
        )
        .await
    }

    /// Admits the Prompt `author` sends to `session_id` at `origin`, as
    /// [`Self::admit_prompt`] admits one here, answering how the Session took
    /// it — or nothing, for a retry that found it admitted already.
    pub(crate) async fn admit_prompt_at(
        &self,
        origin: &Outlook,
        session_id: SessionId,
        request: AdmitPromptRequest,
        author: Author,
    ) -> Result<Option<AdmittedDelivery>, ActRefusal<PromptRefusal>> {
        self.dispatch(
            origin,
            request,
            async |request| match self
                .admit_prompt(session_id, request, Some(author.clone()))
                .await?
            {
                StoreOutcome::Created(admitted) | StoreOutcome::Existing(admitted) => {
                    Ok(admitted.delivery)
                }
            },
            async |name, request| {
                let answered = self
                    .remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/prompts"),
                        Some(&request),
                        &author,
                    )
                    .await
                    .inspect_err(|refusal| {
                        self.note_uncertain(refusal, &author, name, request.prompt.id, false);
                        self.forget_if_gone(refusal, name, session_id);
                    })?;
                self.record_remote_act(&author, name, session_id, false)
                    .await;
                Ok(answered
                    .headers
                    .get(PROMPT_ADMISSION_HEADER)
                    .and_then(|admitted| admitted.to_str().ok())
                    .and_then(AdmittedDelivery::named))
            },
        )
        .await
    }

    /// Interrupts `session_id` at `origin` for `author`, as
    /// [`Self::interrupt_session`] interrupts one here.
    pub(crate) async fn interrupt_session_at(
        &self,
        origin: &Outlook,
        session_id: SessionId,
        author: Author,
    ) -> Result<InterruptOutcome, ActRefusal<InterruptRefusal>> {
        self.dispatch(
            origin,
            (),
            async |()| self.interrupt_session(session_id, Some(&author)).await,
            async |name, ()| {
                let answered = self
                    .remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/interrupt"),
                        None::<&()>,
                        &author,
                    )
                    .await
                    .inspect_err(|refusal| self.forget_if_gone(refusal, name, session_id))?;
                self.record_remote_act(&author, name, session_id, false)
                    .await;
                // Stopping work says everything it has to say by succeeding.
                if answered.is_empty() {
                    Ok(InterruptOutcome::StoppedWork)
                } else {
                    answered.read()
                }
            },
        )
        .await
    }

    /// Sets `session_id` at `origin` aside, or brings it back, for `author`,
    /// as [`Self::settle_session`] does here, answering the summary the change
    /// left standing.
    pub(crate) async fn settle_session_at(
        &self,
        origin: &Outlook,
        session_id: SessionId,
        settled: bool,
        author: Author,
    ) -> Result<SessionSummary, ActRefusal<SettleRefusal>> {
        self.dispatch(
            origin,
            (),
            async |()| {
                self.settle_session(session_id, settled, Some(&author))
                    .await
            },
            async |name, ()| {
                let summary = self
                    .remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/settlement"),
                        Some(&SettleSessionRequest { settled }),
                        &author,
                    )
                    .await
                    .inspect_err(|refusal| self.forget_if_gone(refusal, name, session_id))
                    .and_then(|answered| answered.read())?;
                self.record_remote_act(&author, name, session_id, false)
                    .await;
                Ok(summary)
            },
        )
        .await
    }

    /// Answers the Questionnaire `id` of `session_id` at `origin` for
    /// `author`, as [`Self::answer_questionnaire`] answers one here.
    pub(crate) async fn answer_questionnaire_at(
        &self,
        origin: &Outlook,
        session_id: SessionId,
        id: QuestionnaireId,
        submission: QuestionnaireSubmission,
        author: Author,
    ) -> Result<(), ActRefusal<AnswerRefusal>> {
        self.dispatch(
            origin,
            submission,
            async |submission| {
                self.answer_questionnaire(session_id, id, submission, Some(author.clone()))
                    .await
            },
            async |name, submission| {
                self.remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/questionnaires/{id}"),
                        Some(&submission),
                        &author,
                    )
                    .await
                    .inspect_err(|refusal| self.forget_if_gone(refusal, name, session_id))?;
                self.record_remote_act(&author, name, session_id, false)
                    .await;
                Ok(())
            },
        )
        .await
    }

    /// Records the act `author` just had the Remote `remote` perform on its
    /// Session `session_id` — beginning it there, where `began` — where this
    /// Server's Sidekick performed it, and has that Remote read again for
    /// every tree that now lists it.
    async fn record_remote_act(
        &self,
        author: &Author,
        remote: &str,
        session_id: SessionId,
        began: bool,
    ) {
        if let Some(sidekick) = author.sidekick_session() {
            // An act on a Subagent's Session there stands by the Session
            // heading it, as one on this Server's does; a Session just begun
            // heads its own. Where the Remote does not say which heads it,
            // the act is kept unresolved and asked again when it is next
            // read, never taken as one on a Session it may not list.
            let (session_id, resolved) = if began {
                (session_id, true)
            } else {
                match self.remote_top_level(remote, session_id).await {
                    Some(top_level) => (top_level, true),
                    None => (session_id, false),
                }
            };
            self.sessions
                .record_remote_sidekick_act(sidekick, remote, session_id, began, resolved);
            self.keep_remote_in_view(remote, true);
        }
    }

    /// Forgets every act on the Session `session_id` of the Remote `remote`
    /// where `refusal` is the Remote's saying it holds no such Session.
    fn forget_if_gone(&self, refusal: &RemoteActRefusal, remote: &str, session_id: SessionId) {
        if refusal.is_session_not_found() {
            self.sessions.forget_remote_session(remote, session_id);
        }
    }

    /// Holds the act carrying the Prompt `prompt` that `author` asked of the
    /// Remote `remote` as uncertain, where `refusal` says it may have been
    /// done all the same, so a read finding the Prompt there records it.
    fn note_uncertain(
        &self,
        refusal: &RemoteActRefusal,
        author: &Author,
        remote: &str,
        prompt: PromptId,
        began: bool,
    ) {
        if let Some(sidekick) = author.sidekick_session()
            && refusal.may_have_acted()
        {
            self.uncertain.note(UncertainAct {
                sidekick,
                remote: remote.to_owned(),
                prompt,
                began,
            });
        }
    }

    /// Records each act still uncertain that the Session `snapshot`, just
    /// read from the Remote `remote`, shows was done there.
    pub(super) fn settle_uncertain_acts(&self, remote: &str, snapshot: &SessionSnapshot) {
        let done = self.uncertain.done_in(remote, &snapshot.prompts);
        // A Prompt is only ever a top-level Session's, so the Session read
        // heads its own tree.
        for act in &done {
            self.sessions.record_remote_sidekick_act(
                act.sidekick,
                remote,
                snapshot.session.id,
                act.began,
                true,
            );
        }
        if !done.is_empty() {
            self.keep_remote_in_view(remote, true);
        }
    }

    /// Sets the Description of a Workspace the Remote `name` lists, as the
    /// user's own setting of it there does, for `author`.
    pub(crate) async fn describe_remote_workspace(
        &self,
        name: &str,
        request: &SetWorkspaceDescriptionRequest,
        author: Author,
    ) -> Result<(), RemoteActRefusal> {
        self.remotes
            .act(
                name,
                Method::POST,
                &format!("{WORKSPACES_PATH}/description"),
                Some(request),
                &author,
            )
            .await
            .map(|_| ())
    }

    /// The Workspaces the Remote `name` knows, read on the way to an act on
    /// one of them: refused as the act would be.
    pub(crate) async fn remote_workspaces(
        &self,
        name: &str,
    ) -> Result<WorkspaceListing, RemoteActRefusal> {
        self.remotes.read_for_act(name, WORKSPACES_PATH).await
    }

    /// The Providers and Models the Remote `name` hosts, as its own Landing
    /// offers them, read on the way to beginning a Session there.
    pub(crate) async fn remote_model_catalog(
        &self,
        name: &str,
    ) -> Result<ModelCatalog, RemoteActRefusal> {
        self.remotes.read_for_act(name, "/v1/models").await
    }

    /// The Agent Selection the Remote `name`'s own Landing holds, where it
    /// holds one, read on the way to beginning a Session there.
    pub(crate) async fn remote_landing_selection(
        &self,
        name: &str,
    ) -> Result<Option<AgentSelection>, RemoteActRefusal> {
        self.remotes
            .read_for_act::<Health>(name, "/health")
            .await
            .map(|health| health.landing_agent_selection)
    }
}
