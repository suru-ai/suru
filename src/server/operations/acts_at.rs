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
//! Nothing is kept to do again. An act at a Remote that cannot be asked is
//! refused saying so, and the Sidekick asks again once the Remote answers.
//!
//! An act a Remote takes is recorded here, against the Sidekick's Session,
//! since only this Server knows both ends of it — and so is what a Sidekick
//! is owed Reports of there, once it sends a Prompt or gives an Answer, which
//! the Remote owes it nothing of (see [`crate::sessions::SessionStore`]'s
//! Remote Reports): the Session acted on — or
//! begun (see [`super::beginnings`]) — stands beneath the Sidekick's Session
//! in its tree from then on, as that Remote says of it (see
//! [`super::remote_entries`]). One whose answer never came back whole may
//! have been done all the same, so it is recorded too, durably, as not yet
//! confirmed, against the Pairing it was carried through: any read of that
//! Remote finding the Session confirms it, and one asked for after its
//! outcome became unknown finding the Remote holds no such Session drops it.

use axum::http::Method;

use super::{
    AdmittedDelivery, AnswerRefusal, InterruptRefusal, PromptRefusal, SessionOperations,
    SettleRefusal,
    origins::{RemoteActRefusal, SESSIONS_PATH, WORKSPACES_PATH},
};
use crate::protocol::{
    ActId, AdmitPromptRequest, AgentSelection, Author, Health, InterruptOutcome, ModelCatalog,
    Outlook, PROMPT_ADMISSION_HEADER, PromptId, QuestionnaireId, QuestionnaireSubmission,
    SessionId, SessionSummary, SetWorkspaceDescriptionRequest, SettleSessionRequest,
    WorkspaceListing,
};
use crate::sessions::{
    ConfirmedBeginning, RemoteAct, RemoteContribution, RemoteOwing, StoreOutcome,
};

/// Why an act at an Origin was refused: as this Server refuses it, in the
/// refusal `R` its own operation gives, or as a Remote did — or because the
/// Remote could not be asked.
#[derive(Debug)]
pub(crate) enum ActRefusal<R> {
    Here(R),
    There(RemoteActRefusal),
}

/// The Session an act is on, who performs it, and what — carried to a
/// Remote — it is owed Reports of there, where it sets work going.
struct Acting<'a> {
    session_id: SessionId,
    author: &'a Author,
    owes: Option<Owes>,
}

/// What an act that sets work going asks of a Session.
#[derive(Clone, Copy)]
enum Owes {
    /// A Prompt it sends, by the identity it was admitted under.
    Prompt(PromptId),
    /// An Answer it gives the Questionnaire.
    Answer(QuestionnaireId),
}

impl SessionOperations {
    /// Performs the act `act` at `origin`: as `here` performs it on this
    /// Server, refused in its own words, or as `there` carries it to the
    /// Remote `origin` names, through the Pairing whose key fingerprint it is
    /// handed, its outcome recorded against the Session `acting` names there
    /// — and, where it sets work going there, Reports owed of it.
    async fn dispatch<A, T, R>(
        &self,
        origin: &Outlook,
        act: A,
        acting: Acting<'_>,
        here: impl AsyncFnOnce(A) -> Result<T, R>,
        there: impl AsyncFnOnce(&str, ActId, A) -> Result<T, RemoteActRefusal>,
    ) -> Result<T, ActRefusal<R>> {
        match origin {
            Outlook::Local => here(act).await.map_err(ActRefusal::Here),
            Outlook::Remote(name) => {
                let pairing = self.pairing_of(name);
                // Named so, what the act leaves there is told for its own.
                let act_id = ActId::new();
                let outcome = there(name, act_id, act).await;
                self.owe_remote_outcome(&outcome, &acting, name, &pairing, act_id);
                self.record_remote_outcome(
                    &outcome,
                    acting.author,
                    name,
                    &pairing,
                    acting.session_id,
                )
                .await;
                outcome.map_err(ActRefusal::There)
            }
        }
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
        let prompt_id = request.prompt.id;
        self.dispatch(
            origin,
            request,
            Acting {
                session_id,
                author: &author,
                owes: Some(Owes::Prompt(prompt_id)),
            },
            async |request| match self
                .admit_prompt(session_id, request, Some(author.clone()))
                .await?
            {
                StoreOutcome::Created(admitted) | StoreOutcome::Existing(admitted) => {
                    Ok(admitted.delivery)
                }
            },
            async |name, act, request| {
                let answered = self
                    .remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/prompts"),
                        Some(&request),
                        &author,
                        act,
                    )
                    .await?;
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
            Acting {
                session_id,
                author: &author,
                owes: None,
            },
            async |()| self.interrupt_session(session_id, Some(&author)).await,
            async |name, act, ()| {
                let answered = self
                    .remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/interrupt"),
                        None::<&()>,
                        &author,
                        act,
                    )
                    .await?;
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
            Acting {
                session_id,
                author: &author,
                owes: None,
            },
            async |()| {
                self.settle_session(session_id, settled, Some(&author))
                    .await
            },
            async |name, act, ()| {
                self.remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/settlement"),
                        Some(&SettleSessionRequest { settled }),
                        &author,
                        act,
                    )
                    .await
                    .and_then(|answered| answered.read())
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
            Acting {
                session_id,
                author: &author,
                owes: Some(Owes::Answer(id)),
            },
            async |submission| {
                self.answer_questionnaire(session_id, id, submission, Some(author.clone()))
                    .await
            },
            async |name, act, submission| {
                self.remotes
                    .act(
                        name,
                        Method::POST,
                        &format!("{SESSIONS_PATH}/{session_id}/questionnaires/{id}"),
                        Some(&submission),
                        &author,
                        act,
                    )
                    .await
                    .map(|_| ())
            },
        )
        .await
    }

    /// The key fingerprint of the Pairing the Remote `remote` is reached
    /// through now, and nothing where it is paired with none.
    pub(super) fn pairing_of(&self, remote: &str) -> String {
        self.remotes
            .named(remote)
            .map(|paired| paired.fingerprint)
            .unwrap_or_default()
    }

    /// Holds what the Sidekick authoring `acting` is owed of the work it
    /// sets going, where it sets any, as the act `act` it carried to the
    /// Remote `remote` through the Pairing whose key fingerprint is
    /// `pairing`, as `outcome` says: owed, where the Remote took the act;
    /// owing nothing until a read finds it done, where its answer never came
    /// back whole; and nothing, where it was refused or never carried there.
    /// Held before the act is recorded, which has the Remote read again — and
    /// so what is owed of it read at once.
    fn owe_remote_outcome<T>(
        &self,
        outcome: &Result<T, RemoteActRefusal>,
        acting: &Acting<'_>,
        remote: &str,
        pairing: &str,
        act: ActId,
    ) {
        let Some(owes) = acting.owes else {
            return;
        };
        let owed = match owes {
            Owes::Prompt(prompt_id) => RemoteContribution::Prompt(prompt_id),
            Owes::Answer(questionnaire) => RemoteContribution::Answer { questionnaire, act },
        };
        let confirmed = match outcome {
            Ok(_) => true,
            Err(refusal) if refusal.may_have_acted() => false,
            Err(_) => return,
        };
        if let Some(sidekick) = acting.author.sidekick_session() {
            self.sessions.owe_remote_reports(
                sidekick,
                remote,
                pairing,
                RemoteOwing {
                    session_id: acting.session_id,
                    head: None,
                    title: None,
                    contribution: owed,
                    confirmed,
                },
            );
        }
    }

    /// Records the act `author` asked of the Remote `remote`, through the
    /// Pairing whose key fingerprint is `pairing`, on its Session
    /// `session_id`, as `outcome` says: done; or — where its answer never
    /// came back whole — not yet confirmed; or, where the Remote said it
    /// holds no such Session, every act on it forgotten. An act the Remote
    /// refused, or never had, records nothing.
    async fn record_remote_outcome<T>(
        &self,
        outcome: &Result<T, RemoteActRefusal>,
        author: &Author,
        remote: &str,
        pairing: &str,
        session_id: SessionId,
    ) {
        match outcome {
            Ok(_) => {
                self.record_remote_act(author, remote, pairing, session_id)
                    .await;
            }
            Err(refusal) if refusal.may_have_acted() => {
                // Which Session heads it there is asked when the Remote is
                // next read, as for any act whose heading is not known.
                if let Some(sidekick) = author.sidekick_session() {
                    self.sessions.record_remote_sidekick_act(
                        sidekick,
                        remote,
                        session_id,
                        RemoteAct {
                            pairing: pairing.to_owned(),
                            ..RemoteAct::default()
                        },
                    );
                    self.keep_remote_in_view(remote, true);
                }
            }
            Err(refusal) if refusal.is_session_not_found() => {
                self.sessions.forget_remote_session(remote, session_id);
            }
            Err(_) => {}
        }
    }

    /// Records the act `author` just had the Remote `remote` perform,
    /// through the Pairing whose key fingerprint is `pairing`, on its
    /// Session `session_id`, where this Server's Sidekick performed it, and
    /// has that Remote read again for every tree that now lists it.
    async fn record_remote_act(
        &self,
        author: &Author,
        remote: &str,
        pairing: &str,
        session_id: SessionId,
    ) {
        if let Some(sidekick) = author.sidekick_session() {
            // An act on a Subagent's Session there stands by the Session
            // heading it, as one on this Server's does. Where the Remote does
            // not say which heads it, the act is kept unresolved and asked
            // again when it is next read, never taken as one on a Session it
            // may not list.
            let (session_id, resolved) = match self.remote_top_level(remote, session_id).await {
                Some(top_level) => (top_level, true),
                None => (session_id, false),
            };
            self.sessions.record_remote_sidekick_act(
                sidekick,
                remote,
                session_id,
                RemoteAct {
                    resolved,
                    confirmed: true,
                    pairing: pairing.to_owned(),
                    ..RemoteAct::default()
                },
            );
            self.keep_remote_in_view(remote, true);
        }
    }

    /// Stands the row leading into each beginning on the Remote `remote` a
    /// read there just confirmed, in the Transcript of the Sidekick's
    /// Session that began it.
    pub(super) fn stand_confirmed_beginnings(
        &self,
        remote: &str,
        confirmed: Vec<ConfirmedBeginning>,
    ) {
        for beginning in confirmed {
            if let Err(error) = self.sessions.stand_remote_subsession_row(
                beginning.sidekick,
                remote,
                beginning.session_id,
                beginning.title,
                beginning.prompt,
            ) {
                tracing::warn!(
                    sidekick = %beginning.sidekick,
                    subsession = %beginning.session_id,
                    "a Remote Subsession's row was not stood: {error:#}"
                );
            }
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
                ActId::new(),
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
