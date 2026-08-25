//! Deriving a Session's Title and Emoji from its first Prompt.
//!
//! A Session is titled with the verbatim text of its first Prompt, which is a
//! real Title rather than a placeholder but rarely a good one. The moment that
//! Prompt is admitted, Suru asks a Provider — through an Errand — for a short
//! line naming the subject and the outcome, with an Emoji to stand beside it,
//! and replaces the Title with what comes back.
//!
//! Which Provider is the `session.title.errand` Setting's answer, read when the
//! derivation begins. Left alone it is the Session's own, at that Provider's
//! declared Errand Selection resolved when the Errand runs, so titling is paid
//! for at the rate that Provider keeps for its own work rather than at the rate
//! of conversing. Turned off, no Errand is asked for at all. Pinned to an Agent
//! Selection, that Provider and Model derive every Session's Title whatever the
//! Session itself uses — including a Session using nothing.
//!
//! Everything here is best-effort by construction. The derivation runs in the
//! background alongside the real first Turn and never blocks or gates it; it is
//! attempted once per Session and never again; and a failure of any kind — a
//! Provider that cannot be reached, a reply that arrives too late, a reply that
//! is not the shape Suru asked for — leaves the Prompt-derived Title standing
//! and reaches the Log and nowhere else.

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{
    errands::ErrandRunner,
    model_catalog::ModelCatalogService,
    protocol::{
        AgentSelection, ProviderId, SessionCatalogChange, SessionId, SettingsSnapshot, TitleErrand,
    },
    provider::ProviderErrand,
};

use super::{SessionStore, emoji::single_emoji};

/// What one derivation yields: the Title Suru will store, and the Emoji that
/// stands beside it when the reply carried a usable one. They travel together
/// everywhere because they are derived together, in one Errand, and land
/// together in one change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DerivedTitle {
    title: String,
    emoji: Option<String>,
}

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

/// What Suru asks an Errand to answer with. One object carrying both the Title
/// and the Emoji, because a Model choosing them together picks a better Emoji
/// than one retrofitting it onto a Title it had no say in — and because two
/// calls would cost twice as much to answer the same question.
///
/// Suru's own validation tolerates extra properties: a Model that volunteers a
/// field Suru did not ask for has still answered the question, and throwing a
/// good Title away over it costs the user more than ignoring it does. The
/// requested schema still rejects them because Codex sends output schemas in
/// strict mode, where every object must do so. Providers that cannot enforce the
/// schema may return them anyway, and Suru will keep ignoring them. A reply
/// missing the Title is what does not deserialize, and it is discarded whole.
#[derive(Debug, Deserialize)]
struct DerivedTitleReply {
    title: String,
    #[serde(default)]
    emoji: Option<String>,
}

/// Derives Sessions' Titles. Cloned into the server's shared state, which is
/// what lets Session creation fork a derivation and return without waiting.
#[derive(Clone)]
pub(crate) struct TitleDerivation {
    errands: ErrandRunner,
    /// The live Model catalog, which is what an Errand Selection is resolved
    /// against — every time one runs, rather than once when the server started.
    models: ModelCatalogService,
    sessions: SessionStore,
    /// The effective Settings in force, read when a derivation begins. A
    /// Setting the user changes governs the Sessions they start next rather
    /// than reaching back into a derivation already under way.
    settings: watch::Receiver<SettingsSnapshot>,
}

impl TitleDerivation {
    pub(crate) fn new(
        errands: ErrandRunner,
        models: ModelCatalogService,
        sessions: SessionStore,
        settings: watch::Receiver<SettingsSnapshot>,
    ) -> Self {
        Self {
            errands,
            models,
            sessions,
            settings,
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
    /// choosing one up front, so it titles that Session like any other.
    ///
    /// Where the Session's own Provider runs the Errand, its Agent Selection
    /// decides the Provider and nothing else. The Model is the Provider's own
    /// business — its declared Errand Selection, resolved when the Errand runs
    /// — because the Model a user converses with is not the one that should be
    /// paid to write six words.
    pub(crate) fn derive(
        &self,
        session_id: SessionId,
        workspace: std::path::PathBuf,
        provider: Option<ProviderId>,
        prompt: &str,
    ) {
        let Some(errand_at) = self.errand_at(session_id, provider) else {
            return;
        };
        // The Title as it stands right now is the one this derivation is
        // derived from, and the only one its answer may replace.
        let Some(derived_from) = self.sessions.title(session_id) else {
            return;
        };
        let prompt = errand_prompt(prompt);
        let errands = self.errands.clone();
        let models = self.models.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let (provider, pinned) = match &errand_at {
                ErrandAt::Provider(provider) => (provider, None),
                ErrandAt::Selection(selection) => (&selection.provider, Some(selection)),
            };
            let Some(selection) = models.resolved_errand_selection(provider, pinned).await else {
                tracing::info!(
                    %session_id,
                    "no Title Errand: `{provider}` offers no Model to run one at"
                );
                return;
            };
            let errand = ProviderErrand {
                prompt,
                schema: reply_schema(),
                selection,
                workspace,
            };
            let answer = match errands.run(errand).await {
                Ok(answer) => answer,
                Err(failure) => {
                    tracing::info!(%session_id, "Title Errand produced no Title: {failure}");
                    return;
                }
            };
            let Some(derived) = derived_title(&answer) else {
                tracing::info!(
                    %session_id,
                    "Title Errand answered outside its schema, so the Prompt-derived Title stands"
                );
                return;
            };
            if !sessions.replace_derived_title(session_id, &derived_from, derived) {
                tracing::debug!(
                    %session_id,
                    "a derived Title was discarded because the Title it was derived from has since changed"
                );
            }
        });
    }

    /// Where this Session's Title Errand goes, as the Setting in force decides,
    /// and `None` where it goes nowhere. Read before anything is spawned, so a
    /// Setting turning derivation off costs no task and no Provider call.
    fn errand_at(&self, session_id: SessionId, provider: Option<ProviderId>) -> Option<ErrandAt> {
        match &self.settings.borrow().settings.session.title.errand {
            TitleErrand::Off => {
                tracing::debug!(%session_id, "no Title Errand: Title derivation is turned off");
                None
            }
            TitleErrand::Pinned(selection) => Some(ErrandAt::Selection(selection.clone())),
            TitleErrand::FollowSession => match provider {
                Some(provider) => Some(ErrandAt::Provider(provider)),
                None => {
                    tracing::debug!(
                        %session_id,
                        "no Title Errand: the Session has selected no Provider"
                    );
                    None
                }
            },
        }
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

    /// Replaces a Session's Title and Emoji with a derived pair, but only when
    /// the Title still reads as the one the derivation was derived from.
    ///
    /// The guard is what makes a derivation that is still in flight safe, and
    /// what lets a rename command land later without a schema change: a Title
    /// set by other means while an Errand was outstanding is never overwritten
    /// by the answer to that Errand. Answers `true` when the Title changed.
    ///
    /// The change rides the catalog stream and deliberately does not bump the
    /// Session revision: that revision governs Transcript consistency, and a
    /// Title alters nothing a Transcript reader is holding.
    pub(crate) fn replace_derived_title(
        &self,
        session_id: SessionId,
        derived_from: &str,
        derived: DerivedTitle,
    ) -> bool {
        let DerivedTitle { title, emoji } = derived;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let summary = {
            let Some(record) = state.sessions.get_mut(&session_id) else {
                return false;
            };
            if record.summary.title != derived_from {
                return false;
            }
            record.summary.title = title.clone();
            record.summary.emoji = emoji.clone();
            record.summary.clone()
        };
        self.storage.summary_changed(summary);
        state.publish_catalog_change(SessionCatalogChange::TitleChanged {
            session_id,
            title,
            emoji,
        });
        true
    }
}

/// The Prompt one Title Errand carries: what to write, and the first Prompt to
/// write it about.
fn errand_prompt(prompt: &str) -> String {
    let mut characters = prompt.chars();
    let opening = characters
        .by_ref()
        .take(MAX_ERRAND_PROMPT_CHARS)
        .collect::<String>();
    format!(
        "Name the piece of work the request below begins.\n\n\
         Answer with a title of 3 to 8 words, under 50 characters, naming the subject of \
         the work and what it is meant to achieve. Do not echo the wording of the request, \
         do not address the reader, and do not end with a full stop. Answer also with a \
         single emoji standing for the work, chosen alongside the title rather than fitted \
         to it afterwards.\n\n\
         The request:\n{opening}"
    )
}

/// The shape Suru asks an Errand to answer in. A request rather than a
/// guarantee — every reply is validated here regardless of whether the harness
/// could enforce it.
fn reply_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "description": "3 to 8 words under 50 characters naming the subject of the work and what it is meant to achieve",
            },
            "emoji": {
                "type": "string",
                "description": "a single emoji standing for the work",
            },
        },
        "required": ["title", "emoji"],
        "additionalProperties": false,
    })
}

/// The Title and Emoji an Errand's reply yields, or `None` when it yields
/// neither. Sanitizing runs on every path, whether or not the harness was able
/// to enforce the schema, because no Provider can be relied on to have done it.
fn derived_title(answer: &Value) -> Option<DerivedTitle> {
    let reply: DerivedTitleReply = serde_json::from_value(answer.clone()).ok()?;
    Some(DerivedTitle {
        title: sanitized_title(&reply.title)?,
        emoji: reply.emoji.as_deref().and_then(single_emoji),
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
        assert!(derived_title(&json!({ "emoji": "\u{1F680}" })).is_none());
        assert!(derived_title(&json!("Fix the flicker")).is_none());
        assert!(derived_title(&json!({ "title": "   " })).is_none());
    }

    #[test]
    fn a_reply_with_an_unusable_emoji_still_yields_its_title() {
        assert_eq!(
            derived_title(&json!({ "title": "Fix the flicker", "emoji": "not an emoji" })),
            Some(DerivedTitle {
                title: "Fix the flicker".to_owned(),
                emoji: None,
            })
        );
    }

    #[test]
    fn a_reply_carrying_more_than_suru_asked_for_still_yields_its_title() {
        assert_eq!(
            derived_title(&json!({
                "title": "Fix the flicker",
                "emoji": "\u{1F680}",
                "confidence": 0.9,
            })),
            Some(DerivedTitle {
                title: "Fix the flicker".to_owned(),
                emoji: Some("\u{1F680}".to_owned()),
            })
        );
    }

    #[test]
    fn a_title_errand_schema_rejects_unasked_properties() {
        assert_eq!(reply_schema()["additionalProperties"], json!(false));
    }

    /// The guard is exercised here rather than at the server seam because
    /// nothing else in Suru writes a Title yet — a rename command is the caller
    /// this exists for, and it does not exist. The guard is built now anyway,
    /// because it is what lets that command land later without a schema change
    /// and what makes a derivation still in flight safe.
    #[tokio::test]
    async fn a_derived_title_replaces_only_the_title_it_was_derived_from() {
        use crate::{
            protocol::{CreateSessionRequest, InitialPrompt, PromptId, Workspace},
            storage::{StorageRepository, StorageWriter},
        };

        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(Default::default(), storage);
        let created = store
            .create(CreateSessionRequest {
                agent_selection: None,
                workspace: Workspace {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Explain the seam".to_owned(),
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
                    emoji: None,
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
                emoji: Some("\u{1F9F5}".to_owned()),
            },
        ));
        assert_eq!(
            store.title(session_id).as_deref(),
            Some("Explain the Provider seam")
        );
    }

    #[test]
    fn an_errand_carries_only_the_opening_of_a_long_prompt() {
        let prompt = "x".repeat(MAX_ERRAND_PROMPT_CHARS + 500);
        let carried = errand_prompt(&prompt);
        assert!(carried.contains(&"x".repeat(MAX_ERRAND_PROMPT_CHARS)));
        assert!(!carried.contains(&"x".repeat(MAX_ERRAND_PROMPT_CHARS + 1)));
    }
}
