//! Deriving a Session's Title, and its Workspace's Icon, from its first
//! Prompt.
//!
//! A Session is titled with the verbatim text of its first Prompt, which is a
//! real Title rather than a placeholder but rarely a good one. The moment that
//! Prompt is admitted, Suru asks a Provider — through an Errand — for a short
//! line naming the subject and the outcome, with an Icon standing for the
//! work chosen alongside it, and replaces the Title with what comes back. On
//! the same task, right after, a second Errand asks for an Icon standing for
//! the Workspace itself and a Description of it, where it still lacks either
//! — see [`super::workspace_icon`] for what that Errand asks and how its
//! answer commits.
//!
//! The Session's own Icon travels with its Title but keeps its own rule: a
//! derived Icon lands only where the Session still carries none, so an Icon
//! derived earlier — or chosen by the user, once that lands — is never
//! overwritten by a later derivation. The Title keeps its own, separate rule
//! of replacing only the Title it was derived from. See the **Icon** and
//! **Icon Catalog** glossary entries and ADR 0028 for why an Icon is carried
//! as a Catalog name rather than a codepoint.
//!
//! Which Provider runs both Errands is the `derivation.errand` Setting's
//! answer, read once when derivation begins and shared by the Title and the
//! Workspace's Icon and Description alike. Left alone it is the Session's own, at that
//! Provider's declared Errand Selection resolved when each Errand runs, so
//! deriving either is paid for at the rate that Provider keeps for its own
//! work rather than at the rate of conversing. Turned off, neither Errand is
//! asked for. Pinned to an Agent Selection, that Provider and Model derive
//! every Session's Title and every Workspace's Icon and Description, whatever the Session
//! itself uses — including a Session using nothing.
//!
//! The two Errands run in a fixed order on the one spawned task: the Title
//! Errand first, all the way to its own commit or failure, and only then the
//! Workspace Errand — never concurrently, and never Workspace-first. A harness
//! answering Errands in the order it receives them can therefore always tell
//! the two apart by position, which is what keeps this ordering a documented
//! guarantee rather than an accident of scheduling.
//!
//! A Session admitted from a fresh Managed Worktree preparation is also told
//! the branch that preparation created and the Worktree it belongs to, and
//! only then does its Title Errand ask for a branch name too: 2 to 5 plain
//! words describing the requested work, carried by the same reply as the Title
//! and Icon. The Model reads the Prompt exactly as the Title is asked about
//! it, Skill names included, but is asked not to make a Skill the subject.
//! A proposal Suru can use is stripped of any `refs/heads/` or `suru/` it was
//! spelled with, shaped by the one name-shaping function every Managed
//! Worktree name goes through, and handed to source control, which renames
//! the branch on the owning Server — see
//! [`crate::source_control::SourceControlService::rename_branch`] for the
//! locks it takes and when it declines: a branch whose Worktree has left it,
//! that has an upstream configured, or that a retained preparation intent
//! still names keeps its first name. The rename runs on this same task once
//! the Title Errand's answer is committed, alongside the Workspace Errand
//! rather than in front of it or behind it: the rename may wait out the first
//! Turn's hold on the Repository, and the Workspace Errand never waits for
//! that, just as the rename never waits for the Workspace Errand. The Worktree's
//! location keeps the name it was created with. The rename is recorded as the
//! Checkout State of every Session sharing the Worktree before source
//! control's locks are let go, so their recovery facts name the new branch at
//! once and clients hear of it as they hear of any other reading. Every other
//! Session's Errand — one working in an existing Worktree, or in none — asks
//! for no branch at all.
//!
//! Everything here is best-effort by construction. Derivation runs in the
//! background alongside the real first Turn and never blocks or gates it; the
//! Title is attempted once per Session and never again, so its branch is too;
//! and a failure of any kind — a Provider that cannot be reached, a reply that
//! arrives too late, a reply that is not the shape Suru asked for, a branch
//! source control could not rename — leaves the Prompt-derived Title standing,
//! the Workspace without an Icon or a Description, or the branch with the name its preparation
//! gave it, and reaches the Log and nowhere else. A reply whose Title is good
//! but whose branch is missing or unusable lands its Title and Icon and
//! renames nothing, exactly as an unusable Icon leaves its Title to land alone.

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{
    errands::ErrandRunner,
    icon_catalog,
    model_catalog::ModelCatalogService,
    protocol::{
        AgentSelection, AttachmentBinding, DerivationErrand, Prompt, ProviderId,
        SessionCatalogChange, SessionChange, SessionId, SessionSummary, SettingsSnapshot,
        SkillInvocation, Workspace,
    },
    provider::ProviderErrand,
    source_control::{BranchRename, CreatedBranch, PreparationStore, SourceControlService, naming},
};

use super::{SessionStore, workspace_icon};

/// The most of a first Prompt an Errand carries. The opening is taken rather
/// than the ending because a first Prompt states its intent up front and trails
/// off into detail, and titling should not cost a full-context call.
const MAX_ERRAND_PROMPT_CHARS: usize = 2_000;

/// The most characters a stored Title runs to, ellipsis included. The Errand
/// asks for well under 50, so the gap is deliberate: a Title that reaches this
/// cap reads as a bug rather than as normal.
///
/// The ellipsis counts toward the cap here, unlike
/// [`crate::provider::concise_remote_message`], which appends it beyond one.
/// The difference is what the cap is for: a Title is the cell a Session picker
/// draws and lays out against, so what it costs a row is the whole of it,
/// while a Provider-authored failure is only bounded against a Provider that
/// will not stop talking.
const MAX_TITLE_CHARS: usize = 80;

/// The pairs a Model may wrap a Title in when it answers with one. Stripped in
/// matched pairs only, so a Title that legitimately opens with a quote keeps it.
const QUOTE_PAIRS: [(char, char); 6] = [
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
    ('\u{201C}', '\u{201D}'),
    ('\u{2018}', '\u{2019}'),
    ('\u{00AB}', '\u{00BB}'),
];

/// What Suru asks an Errand to answer with. One object carrying both the
/// Title and the Icon Catalog name, because a Model choosing them together
/// picks a better Icon than one retrofitting it onto a Title it had no say
/// in — and because two calls would cost twice as much to answer the same
/// question.
///
/// Suru's own validation tolerates extra properties and a missing or unusable
/// Icon: a Model that volunteers a field Suru did not ask for, or names an
/// Icon Suru's Catalog does not carry, has still answered the Title question,
/// and throwing a good Title away over it costs the user more than ignoring
/// it does. The requested schema still marks every property required and
/// rejects unasked ones because Codex sends output schemas in strict mode,
/// where every property must be required and no others allowed. Providers
/// that cannot enforce the schema may return outside it anyway, and Suru will
/// keep validating regardless. A reply missing the Title is what does not
/// deserialize at all, and it is discarded whole.
///
/// The branch a fresh Managed Worktree's Errand also asks for is held to the
/// same leniency, and further: it is read as whatever JSON arrives, so even a
/// branch of the wrong type costs the reply nothing but its branch.
#[derive(Debug, Deserialize)]
struct DerivedTitleReply {
    title: String,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    branch: Value,
}

/// What one derivation yields: the Title Suru will store, and the Icon
/// Catalog name that stands beside it when the reply named a usable one.
/// They travel together everywhere because they are derived together, in one
/// Errand, and land together in one change — though the Icon lands only
/// where the Session still has none.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DerivedTitle {
    title: String,
    icon: Option<String>,
}

/// What one Title Errand's reply yields: the Title and Icon, and the branch
/// fragment it proposed where it proposed a usable one.
#[derive(Clone, Debug, Eq, PartialEq)]
struct DerivedReply {
    title: DerivedTitle,
    branch: Option<String>,
}

/// Derives Sessions' Titles and Icons, Workspaces' Icons, and fresh Managed
/// Worktrees' branch names. Cloned into the server's shared state, which is
/// what lets Session creation fork a derivation and return without waiting.
#[derive(Clone)]
pub(crate) struct Derivation {
    errands: ErrandRunner,
    /// The live Model catalog, which is what an Errand Selection is resolved
    /// against — every time one runs, rather than once when the server started.
    models: ModelCatalogService,
    sessions: SessionStore,
    /// The effective Settings in force, read when a derivation begins. A
    /// Setting the user changes governs the Sessions they start next rather
    /// than reaching back into a derivation already under way.
    settings: watch::Receiver<SettingsSnapshot>,
    /// This owning Server's source control, which renames a fresh Managed
    /// Worktree's branch under the locks it shares with preparation.
    source_control: SourceControlService,
    preparations: PreparationStore,
}

impl Derivation {
    pub(crate) fn new(
        errands: ErrandRunner,
        models: ModelCatalogService,
        sessions: SessionStore,
        settings: watch::Receiver<SettingsSnapshot>,
        source_control: SourceControlService,
        preparations: PreparationStore,
    ) -> Self {
        Self {
            errands,
            models,
            sessions,
            settings,
            source_control,
            preparations,
        }
    }

    /// Forks a derivation for a Session whose first Prompt has just been
    /// admitted, and returns at once. The derivation is independent of the
    /// first Turn's lifetime — interrupting or failing that Turn does not
    /// cancel it, because the Prompt was still written and still deserves a
    /// Title.
    ///
    /// Following the Session, a Session that has selected no Provider is
    /// skipped, permanently: Suru will not pick a Provider the user did not
    /// choose, and deferring the attempt would give derivation a second trigger
    /// point and pending state to carry. A pinned Selection is the user
    /// choosing one up front, so it titles that Session like any other. The
    /// same gate governs the Workspace Errand below: a Setting or a Session
    /// that skips the Title skips the Workspace's Icon and Description too, since all are one
    /// Provider call asked at one moment.
    ///
    /// Where the Session's own Provider runs the Errand, its Agent Selection
    /// decides the Provider and nothing else. The Model is the Provider's own
    /// business — its declared Errand Selection, resolved when the Errand runs
    /// — because the Model a user converses with is not the one that should be
    /// paid to write six words.
    ///
    /// `workspace` is read once, here, for whether it already carries an Icon
    /// and a Description: a Workspace that carries both skips the second
    /// Errand, and one lacking either gets exactly one attempt from this
    /// Session — a race with another Session created in the same Workspace
    /// before either commits is possible and harmless, because
    /// [`SessionStore::commit_workspace_icon`] and
    /// [`SessionStore::commit_workspace_description`] only ever fill an
    /// absence.
    ///
    /// `created_branch` is the branch a fresh Managed Worktree preparation
    /// made for this Session, and `None` for every other Session: it is what
    /// puts a branch into the Errand's question, and what a usable answer
    /// renames. The same gate governs it, so a Setting or Session that skips
    /// the Title leaves the branch its first name.
    pub(crate) fn derive(
        &self,
        session_id: SessionId,
        execution_directory: std::path::PathBuf,
        provider: Option<ProviderId>,
        prompt: &Prompt,
        workspace: &Workspace,
        created_branch: Option<CreatedBranch>,
    ) {
        let Some(errand_at) = self.errand_at(session_id, provider) else {
            return;
        };
        // The Title as it stands right now is the one this derivation is
        // derived from, and the only one its answer may replace.
        let Some(derived_from) = self.sessions.title(session_id) else {
            return;
        };
        let asks_branch = created_branch.is_some();
        let title_prompt = errand_prompt(
            &prompt.text,
            &prompt.skill_invocations,
            &prompt.attachments,
            asks_branch,
        );
        let workspace_errand =
            (workspace.icon.is_none() || workspace.description.is_none()).then(|| {
                (
                    workspace.id.clone(),
                    workspace_icon::errand_prompt(workspace),
                )
            });
        let errands = self.errands.clone();
        let models = self.models.clone();
        let sessions = self.sessions.clone();
        let source_control = self.source_control.clone();
        let preparations = self.preparations.clone();
        tokio::spawn(async move {
            let (provider, pinned) = match &errand_at {
                ErrandAt::Provider(provider) => (provider, None),
                ErrandAt::Selection(selection) => (&selection.provider, Some(selection)),
            };
            let Some(selection) = models.resolved_errand_selection(provider, pinned).await else {
                tracing::info!(
                    %session_id,
                    "no Errand: `{provider}` offers no Model to run one at"
                );
                return;
            };
            // The Title Errand runs first, to completion, before the
            // Workspace Errand is even built — see the module doc for why the
            // order is a guarantee rather than a scheduling accident.
            let title_errand = ProviderErrand {
                prompt: title_prompt,
                schema: reply_schema(asks_branch),
                selection: selection.clone(),
                execution_directory: execution_directory.clone(),
            };
            let proposed_branch = match errands.run(title_errand).await {
                Ok(answer) => match derived_reply(&answer) {
                    Some(DerivedReply { title, branch }) => {
                        if !sessions.replace_derived_title(session_id, &derived_from, title) {
                            tracing::debug!(
                                %session_id,
                                "a derived Title was discarded because the Title it was derived from has since changed"
                            );
                        }
                        branch
                    }
                    None => {
                        tracing::info!(
                            %session_id,
                            "Title Errand answered outside its schema, so the Prompt-derived Title stands"
                        );
                        None
                    }
                },
                Err(failure) => {
                    tracing::info!(%session_id, "Title Errand produced no Title: {failure}");
                    None
                }
            };
            // The rename may wait out the first Turn's execution lease, and
            // the Workspace Errand on its Provider: neither waits on the other,
            // though both wait on the Title Errand, so no two Errands are ever
            // outstanding at once.
            let rename = async {
                let Some(created) = &created_branch else {
                    return;
                };
                match &proposed_branch {
                    Some(proposal) => {
                        rename_branch(
                            session_id,
                            &source_control,
                            &preparations,
                            &sessions,
                            created,
                            proposal,
                        )
                        .await
                    }
                    None => tracing::info!(
                        %session_id,
                        branch = %created.branch,
                        "no usable branch was derived, so the Managed Worktree keeps its first name"
                    ),
                }
            };
            let workspace_icon = async {
                if let Some((workspace_id, prompt)) = workspace_errand {
                    derive_workspace(
                        &errands,
                        &sessions,
                        workspace_id,
                        ProviderErrand {
                            prompt,
                            schema: workspace_icon::reply_schema(),
                            selection,
                            execution_directory,
                        },
                    )
                    .await
                }
            };
            tokio::join!(rename, workspace_icon);
        });
    }

    /// Where this derivation's Errands go, as the Setting in force decides,
    /// and `None` where they go nowhere. Read before anything is spawned, so a
    /// Setting turning derivation off costs no task and no Provider call.
    fn errand_at(&self, session_id: SessionId, provider: Option<ProviderId>) -> Option<ErrandAt> {
        match &self.settings.borrow().settings.derivation.errand {
            DerivationErrand::Off => {
                tracing::debug!(%session_id, "no Errand: derivation is turned off");
                None
            }
            DerivationErrand::Pinned(selection) => Some(ErrandAt::Selection(selection.clone())),
            DerivationErrand::FollowSession => match provider {
                Some(provider) => Some(ErrandAt::Provider(provider)),
                None => {
                    tracing::debug!(
                        %session_id,
                        "no Errand: the Session has selected no Provider"
                    );
                    None
                }
            },
        }
    }
}

/// Runs a Workspace Errand and commits each half of its answer where the
/// Workspace still lacks it: the Icon where it carries none, the Description
/// where it carries none, neither waiting on the other.
async fn derive_workspace(
    errands: &ErrandRunner,
    sessions: &SessionStore,
    workspace_id: crate::protocol::WorkspaceId,
    errand: ProviderErrand,
) {
    let answer = match errands.run(errand).await {
        Ok(answer) => answer,
        Err(failure) => {
            tracing::info!(
                ?workspace_id,
                "Workspace Errand produced no Icon or Description: {failure}"
            );
            return;
        }
    };
    let derived = workspace_icon::derived_reply(&answer);
    // The Description is attempted ahead of the Icon, so whoever hears of the
    // Icon landing knows the whole reply has been read.
    match derived.description {
        Some(description) => {
            if !sessions.commit_workspace_description(&workspace_id, description) {
                tracing::debug!(
                    ?workspace_id,
                    "a derived Workspace Description was discarded because the Workspace already carries one"
                );
            }
        }
        None => tracing::info!(
            ?workspace_id,
            "Workspace Errand answered with no Description"
        ),
    }
    match derived.icon {
        Some(icon) => {
            if !sessions.commit_workspace_icon(&workspace_id, icon) {
                tracing::debug!(
                    ?workspace_id,
                    "a derived Workspace Icon was discarded because the Workspace already carries one"
                );
            }
        }
        None => tracing::info!(
            ?workspace_id,
            "Workspace Errand answered with no Icon the Icon Catalog carries"
        ),
    }
}

/// Renames a fresh Managed Worktree's branch to a derived proposal, saying in
/// the Log and nowhere else how that went: a branch that keeps its first name
/// is a cosmetic loss, never a failure the Session hears about.
///
/// A rename is recorded as the Checkout State of every Session sharing the
/// Worktree the moment it happens, under source control's locks: their
/// recovery facts name the new branch from then on, and clients hear of it
/// the way they hear of any other reading.
async fn rename_branch(
    session_id: SessionId,
    source_control: &SourceControlService,
    preparations: &PreparationStore,
    sessions: &SessionStore,
    created: &CreatedBranch,
    proposal: &str,
) {
    let record = |reading| {
        sessions
            .record_checkout(reading)
            .map_err(|error| error.to_string())
    };
    match source_control
        .rename_branch(preparations, created, proposal, record)
        .await
    {
        Ok(BranchRename::Renamed { branch }) => tracing::info!(
            %session_id,
            from = %created.branch,
            to = %branch,
            "renamed a Managed Worktree's branch to the derived name"
        ),
        Ok(BranchRename::Unchanged) => tracing::debug!(
            %session_id,
            branch = %created.branch,
            "the derived branch name is the one the Managed Worktree already has"
        ),
        Err(failure) => tracing::info!(
            %session_id,
            branch = %created.branch,
            "the Managed Worktree keeps its first name: {failure}"
        ),
    }
}

/// What one derivation was told to run its Errand at: a Provider whose own
/// declaration decides the Model, or a whole Selection the user pinned. Both
/// are resolved against the live catalog by the same rules when the Errand
/// runs; the difference is only whose choice is being resolved.
enum ErrandAt {
    Provider(ProviderId),
    Selection(AgentSelection),
}

impl SessionStore {
    /// The Session's Title as it stands, or `None` for a Session this server
    /// does not hold.
    pub(crate) fn title(&self, session_id: SessionId) -> Option<String> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions
            .get(&session_id)
            .map(|record| record.summary.title.clone())
    }

    /// Replaces a Session's Title and Icon with a derived pair, but only
    /// where each of their own guards allows it: the Title only when it
    /// still reads as the one the derivation was derived from, and the Icon
    /// only where the Session still carries none.
    ///
    /// The Title guard is what makes a derivation that is still in flight
    /// safe, and what lets a rename command land later without a schema
    /// change: a Title set by other means while an Errand was outstanding is
    /// never overwritten by the answer to that Errand. The Icon guard is the
    /// same idea applied to derivation filling an absence rather than
    /// replacing a value: an Icon this Session already carries — derived
    /// earlier, or chosen by the user once that lands — stands regardless of
    /// what a later derivation offers. Answers `true` when the Title changed;
    /// a Title that changes without an Icon to fill is still a change.
    ///
    /// The change reaches both the open Session stream and the catalog, so
    /// attached clients see the same Title and Icon as readers of Session
    /// listings. [`SessionStore::set_icon`] shares this same
    /// commit-then-publish shape rather than duplicating it, differing only
    /// in that it writes the Icon unconditionally instead of filling an
    /// absence.
    pub(crate) fn replace_derived_title(
        &self,
        session_id: SessionId,
        derived_from: &str,
        derived: DerivedTitle,
    ) -> bool {
        let DerivedTitle { title, icon } = derived;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let icon = {
            let Some(record) = state.sessions.get_mut(&session_id) else {
                return false;
            };
            if record.summary.title != derived_from {
                return false;
            }
            // A derived Icon only ever fills an absence.
            let icon = record.summary.icon.clone().or(icon);
            if let Err(error) = record.commit_derived(
                &self.storage,
                session_id,
                vec![SessionChange::TitleChanged {
                    title: title.clone(),
                    icon: icon.clone(),
                }],
            ) {
                tracing::warn!(%session_id, %error, "Could not commit derived Title");
                return false;
            }
            icon
        };
        state.publish_catalog_change(SessionCatalogChange::TitleChanged {
            session_id,
            title,
            icon,
        });
        state.announce_subagent_tree(session_id);
        true
    }

    /// Sets a Session's Icon to the user's own choice from the Icon Catalog,
    /// replacing whatever it already carried — derived earlier, chosen
    /// before, or absent — because a user's choice always stands rather than
    /// only ever filling an absence the way derivation does. Refuses a name
    /// the Icon Catalog does not carry, so a Session never stores an Icon
    /// that would only ever draw as no Icon at all.
    ///
    /// Shares `replace_derived_title`'s commit-then-publish shape and its
    /// `TitleChanged` change, carrying the Session's current Title unchanged
    /// alongside the new Icon: every client's existing apply path already
    /// repaints from that one change kind, so a chosen Icon reaches it the
    /// same way a derived one does.
    pub(crate) fn set_icon(
        &self,
        session_id: SessionId,
        icon: &str,
    ) -> Result<SessionSummary, SetIconError> {
        if icon_catalog::glyph(icon).is_none() {
            return Err(SetIconError::UnknownIcon);
        }
        let icon = icon.to_owned();
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let (title, summary) = {
            let Some(record) = state.sessions.get_mut(&session_id) else {
                return Err(SetIconError::SessionNotFound);
            };
            let title = record.summary.title.clone();
            record
                .commit_derived(
                    &self.storage,
                    session_id,
                    vec![SessionChange::TitleChanged {
                        title: title.clone(),
                        icon: Some(icon.clone()),
                    }],
                )
                .map_err(|_| SetIconError::Storage)?;
            (title, record.summary.clone())
        };
        state.publish_catalog_change(SessionCatalogChange::TitleChanged {
            session_id,
            title,
            icon: Some(icon),
        });
        Ok(summary)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SetIconError {
    SessionNotFound,
    /// The named Icon does not resolve in the Icon Catalog.
    UnknownIcon,
    Storage,
}

/// The Prompt one Title Errand carries: what to write, and the first Prompt to
/// write it about. `asks_branch` adds the branch name a fresh Managed Worktree
/// is renamed to.
fn errand_prompt(
    prompt: &str,
    skill_invocations: &[SkillInvocation],
    attachments: &[AttachmentBinding],
    asks_branch: bool,
) -> String {
    // Admission already proved each bound `$skill-name` begins at its span and
    // each Attachment label reads as bound at its own. Removing only a Skill's
    // sigil keeps its visible name useful to the title writer without letting
    // a Provider interpret it as an invocation; a label such as `[Image 1]`
    // names nothing of the work, so it goes whole.
    let mut cuts = skill_invocations
        .iter()
        .map(|invocation| invocation.span.start as usize)
        .filter(|start| prompt.as_bytes().get(*start) == Some(&b'$'))
        .map(|start| start..start + 1)
        .chain(
            attachments
                .iter()
                .filter(|binding| prompt.get(binding.span.range()) == Some(&*binding.label))
                .map(|binding| binding.span.range()),
        )
        .collect::<Vec<_>>();
    cuts.sort_unstable_by_key(|cut| (cut.start, cut.end));
    cuts.dedup();
    let mut prompt = prompt.to_owned();
    let mut kept_from = prompt.len();
    for cut in cuts.into_iter().rev() {
        if cut.end <= kept_from {
            kept_from = cut.start;
            prompt.replace_range(cut, "");
        }
    }

    let mut characters = prompt.chars();
    let opening = characters
        .by_ref()
        .take(MAX_ERRAND_PROMPT_CHARS)
        .collect::<String>();
    let branch = if asks_branch {
        " Answer also with a branch name of 2 to 5 plain words describing the requested \
         work, with no prefix and no issue numbers, and never naming a Skill as its subject."
    } else {
        ""
    };
    format!(
        "Name the piece of work the request below begins.\n\n\
         Answer with a title of 3 to 8 words, under 50 characters, naming the subject of \
         the work and what it is meant to achieve. Do not echo the wording of the request, \
         do not address the reader, and do not end with a full stop. Answer also with an \
         Icon standing for the work, chosen from the offered names alongside the title \
         rather than fitted to it afterwards.{branch}\n\n\
         The request:\n{opening}"
    )
}

/// The shape Suru asks an Errand to answer in. A request rather than a
/// guarantee — every reply is validated here regardless of whether the harness
/// could enforce it. Only `asks_branch` puts a `branch` in it, required like
/// every other property for strict mode's sake and validated as leniently as
/// the Icon.
fn reply_schema(asks_branch: bool) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "description": "3 to 8 words under 50 characters naming the subject of the work and what it is meant to achieve",
            },
            "icon": {
                "type": "string",
                "description": "the Icon Catalog name standing for the work",
                "enum": icon_catalog::names(),
            },
        },
        "required": ["title", "icon"],
        "additionalProperties": false,
    });
    if asks_branch {
        schema["properties"]["branch"] = json!({
            "type": "string",
            "description": "2 to 5 plain words describing the requested work, with no prefix, no issue numbers, and no Skill's name as its subject",
        });
        schema["required"] = json!(["title", "icon", "branch"]);
    }
    schema
}

/// What a Title Errand's reply yields: its Title and Icon, and the branch
/// fragment it proposed where that is a string with meaningful words left once
/// shaped — or `None` when it yields no Title, and so nothing at all, its
/// branch included. Sanitizing runs on every path, whether or not the harness
/// was able to enforce the schema, because no Provider can be relied on to
/// have done it.
fn derived_reply(answer: &Value) -> Option<DerivedReply> {
    let reply: DerivedTitleReply = serde_json::from_value(answer.clone()).ok()?;
    Some(DerivedReply {
        title: DerivedTitle {
            title: sanitized_title(&reply.title)?,
            icon: reply
                .icon
                .filter(|name| icon_catalog::glyph(name).is_some()),
        },
        branch: reply.branch.as_str().and_then(naming::proposed_fragment),
    })
}

/// A Model-authored Title as Suru stores it: its first non-empty line, unwrapped
/// from any quotes around it, with its whitespace collapsed and its length
/// capped. A Title that survives none of that is no Title.
fn sanitized_title(raw: &str) -> Option<String> {
    let line = raw.lines().find(|line| !line.trim().is_empty())?;
    let unquoted = unquoted(line.trim());
    let collapsed = unquoted.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= MAX_TITLE_CHARS {
        return Some(collapsed);
    }
    let mut capped = collapsed
        .chars()
        .take(MAX_TITLE_CHARS - 1)
        .collect::<String>();
    capped.push('\u{2026}');
    Some(capped)
}

/// The text inside one matched pair of quotes, or the text itself when it is
/// not wrapped in one.
fn unquoted(text: &str) -> &str {
    let mut characters = text.chars();
    let (Some(first), Some(last)) = (characters.next(), characters.next_back()) else {
        return text;
    };
    if QUOTE_PAIRS
        .iter()
        .any(|(open, close)| first == *open && last == *close)
    {
        characters.as_str().trim()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn derived_title(answer: &Value) -> Option<DerivedTitle> {
        derived_reply(answer).map(|reply| reply.title)
    }

    #[test]
    fn a_title_is_taken_from_the_first_non_empty_line() {
        assert_eq!(
            sanitized_title("\n \nFix the reasoning flicker\nand some rambling after"),
            Some("Fix the reasoning flicker".to_owned())
        );
    }

    #[test]
    fn a_title_loses_the_quotes_around_it_and_its_extra_whitespace() {
        assert_eq!(
            sanitized_title("  \"Fix   the\treasoning flicker\"  "),
            Some("Fix the reasoning flicker".to_owned())
        );
        assert_eq!(
            sanitized_title("\u{201C}Fix the reasoning flicker\u{201D}"),
            Some("Fix the reasoning flicker".to_owned())
        );
    }

    #[test]
    fn a_lone_quote_is_not_a_pair_and_stays() {
        assert_eq!(
            sanitized_title("\"Fix the reasoning flicker"),
            Some("\"Fix the reasoning flicker".to_owned())
        );
    }

    #[test]
    fn an_over_long_title_is_capped_and_marked() {
        let capped = sanitized_title(&"word ".repeat(60)).expect("a long Title still yields one");
        assert_eq!(capped.chars().count(), MAX_TITLE_CHARS);
        assert!(capped.ends_with('\u{2026}'));
    }

    #[test]
    fn a_title_that_exactly_fills_the_cap_is_left_whole() {
        let exact = "w".repeat(MAX_TITLE_CHARS);
        assert_eq!(sanitized_title(&exact), Some(exact));
    }

    #[test]
    fn a_title_of_nothing_is_no_title() {
        assert_eq!(sanitized_title("   \n  \t "), None);
        assert_eq!(sanitized_title("\"\""), None);
    }

    #[test]
    fn a_reply_without_a_title_yields_nothing_at_all() {
        assert!(derived_title(&json!({ "icon": "md-bug" })).is_none());
        assert!(derived_title(&json!("Fix the flicker")).is_none());
        assert!(derived_title(&json!({ "title": "   " })).is_none());
    }

    #[test]
    fn a_reply_with_an_unknown_icon_still_yields_its_title() {
        assert_eq!(
            derived_title(&json!({ "title": "Fix the flicker", "icon": "not-a-catalog-name" })),
            Some(DerivedTitle {
                title: "Fix the flicker".to_owned(),
                icon: None,
            })
        );
    }

    #[test]
    fn a_reply_missing_an_icon_still_yields_its_title() {
        assert_eq!(
            derived_title(&json!({ "title": "Fix the flicker" })),
            Some(DerivedTitle {
                title: "Fix the flicker".to_owned(),
                icon: None,
            })
        );
    }

    #[test]
    fn a_reply_carrying_more_than_suru_asked_for_still_yields_its_title() {
        assert_eq!(
            derived_title(&json!({
                "title": "Fix the flicker",
                "icon": "md-bug",
                "confidence": 0.9,
            })),
            Some(DerivedTitle {
                title: "Fix the flicker".to_owned(),
                icon: Some("md-bug".to_owned()),
            })
        );
    }

    #[test]
    fn a_reply_yields_its_branch_only_where_it_is_usable() {
        let branch = |value: Value| {
            derived_reply(&json!({ "title": "Fix the flicker", "icon": "md-bug", "branch": value }))
                .expect("the Title is derived whatever the branch")
                .branch
        };
        assert_eq!(
            branch(json!("suru/Fix reasoning flicker")),
            Some("fix-reasoning-flicker".to_owned())
        );
        assert_eq!(branch(json!("")), None);
        assert_eq!(branch(json!("please do it")), None);
        assert_eq!(branch(json!(null)), None);
        assert_eq!(branch(json!(42)), None);
        assert_eq!(
            derived_reply(&json!({ "title": "Fix the flicker" }))
                .expect("a reply without a branch still yields its Title")
                .branch,
            None
        );
        assert!(
            derived_reply(&json!({ "branch": "fix-flicker" })).is_none(),
            "a reply without a Title yields no branch either"
        );
    }

    #[test]
    fn only_a_fresh_worktree_is_asked_for_a_branch() {
        let without = reply_schema(false);
        assert!(without["properties"].get("branch").is_none());
        assert_eq!(without["required"], json!(["title", "icon"]));
        assert!(!errand_prompt("Fix the flicker", &[], &[], false).contains("branch"));

        let with = reply_schema(true);
        assert_eq!(with["properties"]["branch"]["type"], json!("string"));
        assert_eq!(
            with["required"],
            json!(["title", "icon", "branch"]),
            "strict mode requires every property the schema names"
        );
        assert_eq!(with["additionalProperties"], json!(false));
        assert!(errand_prompt("Fix the flicker", &[], &[], true).contains("branch name"));
    }

    #[test]
    fn a_title_errand_schema_rejects_unasked_properties() {
        assert_eq!(reply_schema(false)["additionalProperties"], json!(false));
    }

    #[test]
    fn a_title_errand_schema_enumerates_the_icon_catalog() {
        let schema = reply_schema(false);
        assert_eq!(
            schema["properties"]["icon"]["enum"],
            json!(icon_catalog::names()),
            "a Model can only choose an Icon Suru's Catalog can resolve"
        );
        assert_eq!(schema["required"], json!(["title", "icon"]));
    }

    /// The Title guard is exercised here rather than at the server seam
    /// because nothing else in Suru writes a Title yet — a rename command is
    /// the caller this exists for, and it does not exist. The guard is built
    /// now anyway, because it is what lets that command land later without a
    /// schema change and what makes a derivation still in flight safe.
    #[tokio::test]
    async fn a_derived_title_replaces_only_the_title_it_was_derived_from() {
        use crate::{
            protocol::{CreateSessionRequest, InitialPrompt, PromptId},
            storage::{StorageRepository, StorageWriter},
        };

        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let created = store
            .create(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Explain the seam".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create Session");
        let crate::sessions::StoreOutcome::Created(snapshot) = created else {
            panic!("a fresh Prompt creates a Session");
        };
        let session_id = snapshot.session.id;

        assert!(
            !store.replace_derived_title(
                session_id,
                "a Title this Session never had",
                DerivedTitle {
                    title: "Derived from something else".to_owned(),
                    icon: None,
                },
            ),
            "a derivation cannot replace a Title it was not derived from"
        );
        assert_eq!(store.title(session_id).as_deref(), Some("Explain the seam"));

        assert!(store.replace_derived_title(
            session_id,
            "Explain the seam",
            DerivedTitle {
                title: "Explain the Provider seam".to_owned(),
                icon: Some("md-bug".to_owned()),
            },
        ));
        assert_eq!(
            store.title(session_id).as_deref(),
            Some("Explain the Provider seam")
        );
    }

    /// The Icon's own guard: derivation fills an absence and never overwrites
    /// an Icon the Session already carries, exactly as a chosen Icon will
    /// stand against a later derivation once issue #360 lands a way to
    /// choose one.
    #[tokio::test]
    async fn a_derived_icon_fills_an_absence_but_never_overwrites_one() {
        use crate::{
            protocol::{CreateSessionRequest, InitialPrompt, PromptId},
            storage::{StorageRepository, StorageWriter},
        };

        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let created = store
            .create(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Explain the seam".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create Session");
        let crate::sessions::StoreOutcome::Created(snapshot) = created else {
            panic!("a fresh Prompt creates a Session");
        };
        let session_id = snapshot.session.id;

        assert!(store.replace_derived_title(
            session_id,
            "Explain the seam",
            DerivedTitle {
                title: "Explain the Provider seam".to_owned(),
                icon: Some("md-bug".to_owned()),
            },
        ));
        assert_eq!(
            store
                .state
                .lock()
                .expect("Session store lock is not poisoned")
                .sessions
                .get(&session_id)
                .expect("the created Session is held")
                .summary
                .icon
                .as_deref(),
            Some("md-bug"),
            "a first derivation fills the Session's absent Icon"
        );

        assert!(store.replace_derived_title(
            session_id,
            "Explain the Provider seam",
            DerivedTitle {
                title: "Explain the Provider seam once more".to_owned(),
                icon: Some("dev-rust".to_owned()),
            },
        ));
        assert_eq!(
            store
                .state
                .lock()
                .expect("Session store lock is not poisoned")
                .sessions
                .get(&session_id)
                .expect("the created Session is held")
                .summary
                .icon
                .as_deref(),
            Some("md-bug"),
            "a second derivation never overwrites the Icon the Session already carries"
        );
    }

    #[test]
    fn an_errand_carries_only_the_opening_of_a_long_prompt() {
        let prompt = "x".repeat(MAX_ERRAND_PROMPT_CHARS + 500);
        let carried = errand_prompt(&prompt, &[], &[], false);
        assert!(carried.contains(&"x".repeat(MAX_ERRAND_PROMPT_CHARS)));
        assert!(!carried.contains(&"x".repeat(MAX_ERRAND_PROMPT_CHARS + 1)));
    }

    #[test]
    fn an_errand_names_the_work_without_the_labels_of_its_attachments() {
        let text = "$review [Image 1] against [Image 2]";
        let skill = SkillInvocation {
            skill_id: crate::protocol::SkillId::new("review"),
            name: "review".to_owned(),
            scope: None,
            span: crate::protocol::TextSpan { start: 0, end: 7 },
        };
        let label = |label: &str| {
            let start = text.find(label).expect("the label stands in the text");
            AttachmentBinding {
                attachment_id: crate::protocol::AttachmentId::new(label),
                label: label.to_owned(),
                span: crate::protocol::TextSpan::from(start..start + label.len()),
            }
        };
        let carried = errand_prompt(
            text,
            &[skill],
            &[label("[Image 2]"), label("[Image 1]")],
            false,
        );
        assert!(
            carried.ends_with("The request:\nreview  against "),
            "the Skill keeps its name and each label goes whole: {carried}"
        );
    }
}
