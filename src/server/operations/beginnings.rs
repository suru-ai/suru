//! Beginning a Session on a Remote for a Sidekick, so that an outcome never
//! learned can be found and asked again.
//!
//! Every identity the beginning names is chosen here before the Remote is
//! first asked — the Session's own, its first Prompt's, and its Worktree
//! preparation's — and kept, with what the beginning asks for, as an act on
//! that Session not yet confirmed, against the Pairing it is carried
//! through. So a beginning whose answer never came back whole is named to
//! the Sidekick by the Session it is where it was begun, which a read there
//! finds; and asked again by that name it is the very same request — a
//! creation the Remote already took answers with the Session it made, as a
//! Client's retry does, and a preparation it already made resumes — never a
//! second Session. Once the creation was asked for, only the creation is
//! asked again: a Session begun in its Worktree no longer needs the
//! preparation, which the Remote let go of as it began it.
//!
//! The act is confirmed once the Remote answers with the Session begun, or a
//! read of that Remote finds it (see [`crate::sessions::SessionStore`]'s
//! reconciliation of Remote reads), and forgotten where the Remote refused
//! the beginning, or never had it.

use std::path::Path;

use axum::http::Method;

use super::{
    SessionOperations,
    origins::{RemoteActRefusal, SESSIONS_PATH},
};
use crate::protocol::{Author, PrepareCheckoutResult, SessionId, SessionSnapshot};
use crate::sessions::{Beginning, RemoteAct};

/// Where the Session API prepares a Worktree for a Session about to begin.
const PREPARE_PATH: &str = "/v1/checkouts/prepare";

/// Why a beginning on a Remote did not answer with the Session begun.
#[derive(Debug)]
pub(crate) enum BeginningRefusal {
    /// Nothing was begun: the Remote refused it, or was never asked.
    NotBegun(RemoteActRefusal),
    /// The new Worktree it was to begin in was made, and is kept there, but
    /// is not ready, for the reason the Remote gave; nothing was begun. The
    /// preparation is `named` so to the Sidekick.
    WorktreeNotReady { error: String, named: String },
    /// The new Worktree it was to begin in was made, and is kept there, and
    /// the Remote refused the beginning itself, or was never asked for it.
    /// The preparation is `named` so to the Sidekick.
    NotBegunInKeptWorktree {
        refusal: RemoteActRefusal,
        named: String,
    },
    /// It may have been begun all the same: what was asked was carried to
    /// the Remote, and its answer never came back whole. Where it was begun,
    /// it is the Session `session_id`.
    Unknown {
        refusal: RemoteActRefusal,
        session_id: SessionId,
    },
    /// A beginning asked again named no Session this Sidekick began there.
    NoSuchBeginning,
    /// A beginning asked again asked for something other than what the
    /// beginning it named asked for.
    Differs,
}

impl SessionOperations {
    /// Begins the Session `beginning` asks for on the Remote `remote`, for
    /// `author`, answering the Session begun — or the one the Remote already
    /// began for the same request. What it asks for is kept, as an act not
    /// yet confirmed, before the Remote is first asked, and again before each
    /// step, so an outcome never learned is found by the Session it names.
    pub(crate) async fn begin_remote_session(
        &self,
        remote: &str,
        mut beginning: Beginning,
        author: Author,
    ) -> Result<SessionSnapshot, BeginningRefusal> {
        let pairing = self
            .remotes
            .named(remote)
            .map(|paired| paired.fingerprint)
            .map_err(|refusal| BeginningRefusal::NotBegun(refusal.into()))?;
        self.keep_beginning(&author, remote, &pairing, &beginning, false);
        let prepared = beginning.prepare.is_some();
        if let (Some(prepare), false) = (&beginning.prepare, beginning.creating) {
            let answered = self
                .remotes
                .act(remote, Method::POST, PREPARE_PATH, Some(prepare), &author)
                .await
                .and_then(|answered| answered.read::<PrepareCheckoutResult>());
            let prepared = match answered {
                Ok(prepared) => prepared,
                Err(refusal) => {
                    return Err(self.settle_refused(&author, remote, &beginning, refusal, false));
                }
            };
            if let Some(error) = prepared.error {
                self.forget_beginning(&author, remote, &beginning);
                return Err(BeginningRefusal::WorktreeNotReady {
                    error,
                    named: beginning.preparation_named.clone().unwrap_or_default(),
                });
            }
            // A preparation already made keeps the Session it was made for,
            // so the beginning names that one from here.
            let intended = prepared.preparation.intended_session;
            if intended != beginning.session_id() {
                self.forget_beginning(&author, remote, &beginning);
                beginning.create.session_id = Some(intended);
            }
            beginning.create.preparation_id = Some(prepared.preparation.id);
            beginning.create.execution_directory = prepared.preparation.destination;
            beginning.creating = true;
            self.keep_beginning(&author, remote, &pairing, &beginning, false);
        } else if !beginning.creating {
            beginning.creating = true;
            self.keep_beginning(&author, remote, &pairing, &beginning, false);
        }
        let answered = self
            .remotes
            .act(
                remote,
                Method::POST,
                SESSIONS_PATH,
                Some(&beginning.create),
                &author,
            )
            .await
            .and_then(|answered| answered.read::<SessionSnapshot>());
        match answered {
            Ok(begun) => {
                if let Some(sidekick) = author.sidekick_session() {
                    self.sessions.record_remote_sidekick_act(
                        sidekick,
                        remote,
                        begun.session.id,
                        RemoteAct {
                            began: true,
                            resolved: true,
                            confirmed: true,
                            pairing,
                            beginning: None,
                        },
                    );
                    let prompt = beginning.create.prompt.text.clone();
                    self.stand_confirmed_beginnings(
                        remote,
                        vec![crate::sessions::ConfirmedBeginning {
                            sidekick,
                            session_id: begun.session.id,
                            title: begun.title.clone(),
                            prompt,
                        }],
                    );
                    self.keep_remote_in_view(remote, true);
                }
                Ok(begun)
            }
            Err(refusal) => {
                Err(self.settle_refused(&author, remote, &beginning, refusal, prepared))
            }
        }
    }

    /// Asks again for the beginning of the Session `session_id` on the
    /// Remote `remote` that `author` asked for before, whose outcome it never
    /// learned, where it asks for the same: in `directory`, first asked
    /// `prompt`. Answers the Session it began — the one the Remote began for
    /// the earlier request, or the one it begins now.
    pub(crate) async fn begin_remote_session_again(
        &self,
        remote: &str,
        session_id: SessionId,
        directory: &Path,
        prompt: &str,
        author: Author,
    ) -> Result<SessionSnapshot, BeginningRefusal> {
        let sidekick = author
            .sidekick_session()
            .ok_or(BeginningRefusal::NoSuchBeginning)?;
        let act = self
            .sessions
            .remote_sidekick_act(sidekick, remote, session_id)
            .filter(|act| act.began)
            .ok_or(BeginningRefusal::NoSuchBeginning)?;
        match act.beginning {
            Some(beginning) => {
                if beginning.directory != directory || beginning.create.prompt.text != prompt {
                    return Err(BeginningRefusal::Differs);
                }
                self.begin_remote_session(remote, beginning, author).await
            }
            // Confirmed already: it is the Session there.
            None => self
                .remotes
                .read_for_act(remote, &format!("{SESSIONS_PATH}/{session_id}"))
                .await
                .map_err(BeginningRefusal::NotBegun),
        }
    }

    /// What a beginning on the Remote `remote` whose step `refusal` refused
    /// comes to — prepared first, where `prepared`: where the step may have
    /// been done all the same, it is kept, not yet confirmed, and its outcome
    /// is unknown; otherwise it is forgotten, and nothing was begun.
    fn settle_refused(
        &self,
        author: &Author,
        remote: &str,
        beginning: &Beginning,
        refusal: RemoteActRefusal,
        prepared: bool,
    ) -> BeginningRefusal {
        if refusal.may_have_acted() {
            // Its creation asked for, it heads its own tree where it was
            // begun, and a read of that Remote asked for after now that does
            // not find it confirms it was not.
            if beginning.creating {
                let pairing = self.pairing_of(remote);
                self.keep_beginning(author, remote, &pairing, beginning, true);
            }
            return BeginningRefusal::Unknown {
                refusal,
                session_id: beginning.session_id(),
            };
        }
        self.forget_beginning(author, remote, beginning);
        match &beginning.preparation_named {
            Some(named) if prepared && beginning.creating => {
                BeginningRefusal::NotBegunInKeptWorktree {
                    refusal,
                    named: named.clone(),
                }
            }
            _ => BeginningRefusal::NotBegun(refusal),
        }
    }

    /// Keeps the beginning `beginning` that `author` asked of the Remote
    /// `remote`, through the Pairing whose key fingerprint is `pairing`, as
    /// an act not yet confirmed on the Session it names — known to head its
    /// own tree, where `asked`: its creation was asked for and not answered.
    /// Until then it stands in no tree, and nothing judges it gone.
    fn keep_beginning(
        &self,
        author: &Author,
        remote: &str,
        pairing: &str,
        beginning: &Beginning,
        asked: bool,
    ) {
        let Some(sidekick) = author.sidekick_session() else {
            return;
        };
        self.sessions.record_remote_sidekick_act(
            sidekick,
            remote,
            beginning.session_id(),
            RemoteAct {
                began: true,
                resolved: asked,
                confirmed: false,
                pairing: pairing.to_owned(),
                beginning: Some(beginning.clone()),
            },
        );
    }

    /// Forgets the beginning `beginning` that `author` asked of the Remote
    /// `remote`, which the Remote refused, or never had.
    fn forget_beginning(&self, author: &Author, remote: &str, beginning: &Beginning) {
        if let Some(sidekick) = author.sidekick_session() {
            self.sessions
                .forget_remote_sidekick_act(sidekick, remote, beginning.session_id());
        }
    }
}
