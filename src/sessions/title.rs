//! Deriving a Session's Title and Emoji from its first Prompt.
//!
//! A Session is titled with the verbatim text of its first Prompt, which is a
//! real Title rather than a placeholder but rarely a good one. The moment that
//! Prompt is admitted, Suru asks the Session's own Provider — through an Errand
//! — for a short line naming the subject and the outcome, with an Emoji to
//! stand beside it, and replaces the Title with what comes back.
//!
//! Everything here is best-effort by construction. The derivation runs in the
//! background alongside the real first Turn and never blocks or gates it; it is
//! attempted once per Session and never again; and a failure of any kind — a
//! Provider that cannot be reached, a reply that arrives too late, a reply that
//! is not the shape Suru asked for — leaves the Prompt-derived Title standing
//! and reaches the Log and nowhere else.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    errands::ErrandRunner,
    protocol::{AgentSelection, SessionCatalogChange, SessionId},
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
/// Extra properties are tolerated rather than rejected: a Model that volunteers
/// a field Suru did not ask for has still answered the question, and throwing a
/// good Title away over it costs the user more than ignoring it does. A reply
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
    sessions: SessionStore,
}

impl TitleDerivation {
    pub(crate) fn new(errands: ErrandRunner, sessions: SessionStore) -> Self {
        Self { errands, sessions }
    }

    /// Forks a derivation for a Session whose first Prompt has just been
    /// admitted, and returns at once. The derivation is independent of the
    /// first Turn's lifetime — interrupting or failing that Turn does not
    /// cancel it, because the Prompt was still written and still deserves a
    /// Title.
    ///
    /// A Session that has selected no Provider is skipped, permanently: Suru
    /// will not pick a Provider the user did not choose, and deferring the
    /// attempt would give derivation a second trigger point and pending state
    /// to carry.
    pub(crate) fn derive(
        &self,
        session_id: SessionId,
        workspace: std::path::PathBuf,
        selection: Option<AgentSelection>,
        prompt: &str,
    ) {
        let Some(selection) = selection else {
            tracing::debug!(
                %session_id,
                "no Title Errand: the Session has selected no Provider"
            );
            return;
        };
        // The Title as it stands right now is the one this derivation is
        // derived from, and the only one its answer may replace.
        let Some(derived_from) = self.sessions.title(session_id) else {
            return;
        };
        let errand = ProviderErrand {
            prompt: errand_prompt(prompt),
            schema: reply_schema(),
            selection,
            workspace,
        };
        let errands = self.errands.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
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
