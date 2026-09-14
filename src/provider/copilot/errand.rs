//! One Errand on the Copilot CLI, run through a Copilot Session Suru starts and discards.
//!
//! Copilot's harness has no one-shot mode and no way to be handed an output schema, so this is
//! ADR 0011's escape hatch taken for real, and the only way Copilot runs an Errand: it opens a
//! Copilot Session of its own, delivers the one Prompt with the shape written into it, waits for
//! the loop to go idle, and reads the answer off the Session's last agent Message. That the answer
//! came out of a Session is this module's business alone — the caller asks for an Errand and is
//! given JSON.
//!
//! Two things are true of that Session. It is opened in the least-capable mode the SDK offers — no
//! Tools, no Skills, nothing approved on its behalf, and Copilot's own session store turned off —
//! and it is discarded however the Errand ends, including when the Errand's deadline passes and the
//! whole call is dropped mid-flight. Discarding means deleting: Copilot files a Session on disk and
//! offers it back in its own picker even with the session store turned off, and a Session per
//! derived Title accumulating in the CLI the user works in outside Suru is the failure ADR 0011
//! exists to avoid rather than a trace to shrug at.
//!
//! Nothing here creates a Suru Session: no Session store entry, no Transcript, and no Resume State,
//! so the Copilot Session an Errand ran on can never be listed, opened, or continued.

use github_copilot_sdk::{
    MessageOptions, SessionConfig, SessionEvent, SessionId as CopilotSessionId,
    session::Session as NativeSession, session_events::AssistantMessageData,
};
use serde_json::Value;

use super::{
    COPILOT_CLIENT_NAME,
    catalog::tier_id,
    copilot_error,
    session::{lower_selection_options, until_crash},
    transport::CopilotConnection,
};
use crate::provider::{ProviderErrand, ProviderError, harness::SharedHarnessHandle};

/// What a failure running an Errand is reported under: the work Suru asked for rather than the
/// phase of it that went wrong.
const ERRAND_CONTEXT: &str = "Copilot Errand failed";

/// Runs one Errand on the shared harness process `handle` was granted from, and answers with the
/// JSON Copilot replied. The reply is not checked against the Errand's schema here — the schema is
/// a request rather than a guarantee, and whoever asked for the Errand validates it (ADR 0011).
pub(super) async fn run_copilot_errand(
    handle: SharedHarnessHandle<CopilotConnection>,
    errand: ProviderErrand,
) -> Result<Value, ProviderError> {
    let config = errand_session_config(&errand)?;
    let native = until_crash(
        &handle,
        ERRAND_CONTEXT,
        handle.connection().client().create_session(config),
    )
    .await?;
    let mut session = DiscardedSession::holding(native);
    let answered = until_crash(
        &handle,
        ERRAND_CONTEXT,
        session
            .native()
            .send_and_wait(MessageOptions::new(errand_prompt(&errand))),
    )
    .await;
    session.discard().await;
    replied_json(&replied_text(answered?)?)
        .ok_or_else(|| copilot_error(format!("{ERRAND_CONTEXT}: Copilot answered with no JSON")))
}

/// The Copilot Session one Errand runs on: the least-capable, least-persistent Session the SDK will
/// open.
///
/// An Errand is Suru's own work rather than the user's, so the Session it runs on is given nothing
/// to work with: an empty Tool allowlist, no Skills — which are capabilities to reach for rather
/// than instructions to read — and a refusal for anything it asks permission for anyway, against
/// the Session path's full-auto posture. It runs in the Session's own Workspace so that project
/// agent instructions inform the answer, and it streams nothing, because an Errand has no reader to
/// stream to and only the finished reply is of any use.
fn errand_session_config(errand: &ProviderErrand) -> Result<SessionConfig, ProviderError> {
    let options = lower_selection_options(&errand.selection)?;
    let mut config = SessionConfig::default()
        // The SDK registers the identifier before it asks the CLI to create the Session, which is
        // what gives the Session-scoped requests the CLI may issue mid-creation somewhere to land.
        .with_session_id(CopilotSessionId::new(uuid::Uuid::new_v4().to_string()))
        .with_client_name(COPILOT_CLIENT_NAME)
        .with_working_directory(errand.execution_directory.clone())
        .with_model(errand.selection.model.as_str())
        .with_available_tools(Vec::<String>::new())
        .deny_all_permissions()
        // The least-persistent mode on offer. Copilot files the Session under its own state
        // directory and lists it in its own picker regardless, which is why discarding one means
        // deleting it rather than only closing it.
        .with_enable_session_store(false)
        .with_enable_skills(false)
        .with_streaming(false);
    config.reasoning_effort = options.reasoning_effort;
    config.context_tier = options.context_tier.map(tier_id);
    Ok(config)
}

/// The Prompt one Errand delivers: what was asked, and the shape it must be answered in.
///
/// The schema is written into the Prompt because Copilot has no way to be handed one. That makes it
/// advice rather than a constraint, which is why the caller validates what comes back regardless —
/// this only gives a Model that means to comply something to comply with.
fn errand_prompt(errand: &ProviderErrand) -> String {
    format!(
        "{}\n\nAnswer with one JSON object and nothing else — no prose around it, no code fence, \
         and no field the schema does not name. The object must match this JSON Schema:\n{}",
        errand.prompt, errand.schema
    )
}

/// The text of the agent Message an answered Errand settled on. A Session that went idle without
/// ever producing one has answered nothing, which is a failure rather than an empty answer.
fn replied_text(answered: Option<SessionEvent>) -> Result<String, ProviderError> {
    answered
        .and_then(|event| event.typed_data::<AssistantMessageData>())
        .map(|message| message.content)
        .ok_or_else(|| {
            copilot_error(format!(
                "{ERRAND_CONTEXT}: Copilot answered with no message"
            ))
        })
}

/// The JSON object `reply` carries, or nothing when it carries none.
///
/// A reply that is JSON alone is read as it stands. One that wrapped its object in a code fence or
/// a sentence is read from the span between its outermost braces, because a Model that answered the
/// question and then explained itself has still answered the question — and because a Prompt is the
/// only place Copilot could be told the shape at all.
fn replied_json(reply: &str) -> Option<Value> {
    let reply = reply.trim();
    if let Ok(value) = serde_json::from_str(reply) {
        return Some(value);
    }
    let opening = reply.find('{')?;
    let closing = reply.rfind('}')?;
    serde_json::from_str(reply.get(opening..=closing)?).ok()
}

/// The Copilot Session an Errand ran on, discarded however the Errand ends.
///
/// The [`Drop`] arm is the one that matters: an Errand that runs past its deadline is dropped where
/// it stands, and without it the Session it opened would be left behind — which is the whole of
/// what this type is for.
struct DiscardedSession {
    native: Option<NativeSession>,
}

impl DiscardedSession {
    fn holding(native: NativeSession) -> Self {
        Self {
            native: Some(native),
        }
    }

    fn native(&self) -> &NativeSession {
        self.native
            .as_ref()
            .expect("an Errand's Copilot Session is discarded only once the Errand is done with it")
    }

    /// Discards the Session and waits for Copilot to be done with it.
    ///
    /// The discard runs as a task of its own rather than inline, so that an Errand whose deadline
    /// passes while it is tidying up does not take the half-finished tidying with it: a Session
    /// closed but never deleted is one the user is left to find in their own CLI.
    async fn discard(&mut self) {
        let Some(native) = self.native.take() else {
            return;
        };
        if let Some(discarding) = start_discarding(native) {
            let _ = discarding.await;
        }
    }
}

impl Drop for DiscardedSession {
    fn drop(&mut self) {
        let Some(native) = self.native.take() else {
            return;
        };
        // Dropped with the Session still open, which is the Errand having been abandoned before it
        // got as far as discarding. The discard outlives this call rather than being skipped.
        let _ = start_discarding(native);
    }
}

/// Starts the discard on a task of its own, so that whether anything waits for it is the caller's
/// business and dropping the caller never cancels it. Nothing starts when the runtime is already
/// gone, which is the one case nothing can be done about — and it is taking the CLI process with it
/// anyway.
fn start_discarding(native: NativeSession) -> Option<tokio::task::JoinHandle<()>> {
    tokio::runtime::Handle::try_current()
        .map(|runtime| runtime.spawn(discard_session(native)))
        .ok()
}

/// Closes the Copilot Session and then deletes what Copilot filed under it, so an Errand leaves the
/// user's own CLI as it found it. Neither refusal is anything the Errand can act on — the Session is
/// being abandoned either way — so both reach the Log alone.
async fn discard_session(native: NativeSession) {
    let client = native.client().clone();
    let session_id = native.id().clone();
    if let Err(error) = native.disconnect().await {
        tracing::debug!("a Copilot Session opened for an Errand refused to close: {error}");
    }
    if let Err(error) = client.delete_session(&session_id).await {
        tracing::debug!("a Copilot Session opened for an Errand outlived it: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::replied_json;
    use serde_json::json;

    /// Copilot cannot be handed a schema, so a reply that surrounds its answer with the things a
    /// Model surrounds answers with is still an answer. What matters is that the object survives —
    /// whether it is judged the right shape is the caller's question, not this one.
    #[test]
    fn a_json_object_is_read_out_of_whatever_a_model_wrapped_it_in() {
        for wrapped in [
            r#"{"title":"Explain the seam"}"#,
            "  {\"title\":\"Explain the seam\"}\n",
            "```json\n{\"title\":\"Explain the seam\"}\n```",
            "Sure! {\"title\":\"Explain the seam\"} — hope that helps.",
        ] {
            assert_eq!(
                replied_json(wrapped),
                Some(json!({ "title": "Explain the seam" })),
                "{wrapped} carries an answer"
            );
        }
    }

    /// A reply carrying no object at all is nothing the caller could validate, so it is reported as
    /// the failure it is rather than passed on as an answer of some other shape.
    #[test]
    fn a_reply_carrying_no_json_object_is_no_answer() {
        for unusable in [
            "I'd be happy to help with that!",
            "",
            "{\"title\": unquoted}",
        ] {
            assert_eq!(replied_json(unusable), None, "{unusable} answers nothing");
        }
    }
}
