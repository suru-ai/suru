//! Whether the user's Claude Code CLI can be driven at all, asked of the CLI itself before Suru
//! asks it for anything else.
//!
//! Two conditions leave Claude unusable until the user fixes them outside Suru, and each reaches a
//! Model discovery as its own typed reason rather than as a failure: a CLI that is not installed,
//! which the harness machinery reports for every Provider alike, and one no user is signed in to,
//! which the probe reads from the account the init handshake reports. The probe also reads the
//! version: a known version below the one Suru has verified remains usable with advisory guidance,
//! while a CLI too old to answer that question remains incompatible.
//!
//! The probe is a short-lived stream-json process of its own, launched with the CLI's filesystem
//! settings unloaded, no MCP servers started, and no conversation written to disk, and it never
//! delivers a Prompt: it asks two control requests and stops, so a probe can never start a billable
//! Turn.
//!
//! A verdict with no compatibility guidance keeps for a TTL, so catalog reads in quick succession
//! cost one probe between them. An unavailable or advisory verdict is never kept: the user is
//! expected to be fixing it now, and recovery is a catalog refresh and nothing more.

use std::{
    ffi::{OsStr, OsString},
    sync::{Arc, Mutex as StdMutex},
};

use tokio::time::{Duration, Instant};

use super::{
    claude_error, claude_error_context,
    transport::{ClaudeConnection, ClaudeSettingSources, StreamJsonTransport},
    wire::{ControlRequest, NativeAccount, NativeBinaryVersion, NativeInitialize},
};
use crate::{
    protocol::ProviderUnavailability,
    provider::{ProviderError, harness::ProcessRegistry, version::SuggestedCliVersion},
};

/// The Claude Code CLI whose stream-json behavior this build is verified against (ADR 0010).
const CLAUDE_SUGGESTED_VERSION: SuggestedCliVersion =
    SuggestedCliVersion::new("Claude Code CLI", 2, 1, 237);

/// How long a verdict that Claude is usable stands before it is asked of the CLI again.
const AVAILABILITY_TTL: Duration = Duration::from_secs(300);

/// What the probe adds to the stream-json arguments every Claude process is launched with: no MCP
/// servers beyond the ones it was given — which is none — and no conversation left on disk. The
/// CLI's own filesystem settings, and so the hooks they configure, are already unloaded.
const PROBE_ARGS: [&str; 2] = ["--strict-mcp-config", "--no-session-persistence"];

/// What the CLI names the API backend an Anthropic login authenticates against. Every other backend
/// holds its credentials outside the CLI entirely.
const FIRST_PARTY_BACKEND: &str = "firstParty";

/// What a source field reads as when there is no credential behind it.
const NO_SOURCE: &str = "none";

/// The standing answer to whether Claude can be used, and how long the last one is good for.
#[derive(Clone, Debug)]
pub(super) struct ClaudeAvailability {
    ttl: Duration,
    /// When the last warning-free usable verdict stops standing.
    usable_until: Arc<StdMutex<Option<Instant>>>,
}

impl ClaudeAvailability {
    pub(super) fn new() -> Self {
        Self {
            ttl: AVAILABILITY_TTL,
            usable_until: Arc::new(StdMutex::new(None)),
        }
    }

    /// Overrides how long a verdict stands; injectable so tests can watch one expire without
    /// waiting out the default.
    pub(super) fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }

    /// Comes back with advisory compatibility guidance once Claude is usable, and with the typed
    /// condition when it is not.
    pub(super) async fn verify(
        &self,
        executable: &OsStr,
        processes: &ProcessRegistry,
        request_timeout: Duration,
    ) -> Result<Option<String>, ProviderError> {
        if self.verdict_stands() {
            return Ok(None);
        }
        let warning = probe(executable, processes.clone(), request_timeout).await?;
        if warning.is_none() {
            *self
                .usable_until
                .lock()
                .expect("Claude availability lock is not poisoned") =
                Some(Instant::now() + self.ttl);
        }
        Ok(warning)
    }

    fn verdict_stands(&self) -> bool {
        self.usable_until
            .lock()
            .expect("Claude availability lock is not poisoned")
            .is_some_and(|until| Instant::now() < until)
    }
}

/// Asks one short-lived CLI process everything a verdict needs, and stops it again.
async fn probe(
    executable: &OsStr,
    processes: ProcessRegistry,
    request_timeout: Duration,
) -> Result<Option<String>, ProviderError> {
    let ClaudeConnection { transport, process } = StreamJsonTransport::launch(
        executable,
        PROBE_ARGS.iter().map(OsString::from),
        None,
        None,
        processes,
        ClaudeSettingSources::Isolated,
    )
    .await?;
    let verdict = ask_for_a_verdict(&transport, request_timeout).await;
    transport.close().await;
    // The verdict is the answer the probe was launched for, so a process that then stops badly only
    // has a story to tell when there is no verdict to report.
    let stopped = process.wait_until_stopped().await;
    match verdict {
        Err(error) => Err(error),
        Ok(warning) => {
            stopped?;
            Ok(warning)
        }
    }
}

/// Everything the CLI has to answer before Suru will drive it.
async fn ask_for_a_verdict(
    transport: &StreamJsonTransport,
    request_timeout: Duration,
) -> Result<Option<String>, ProviderError> {
    let warning = verify_version(transport, request_timeout).await?;
    verify_signed_in(transport, request_timeout).await?;
    Ok(warning)
}

/// Returns advisory guidance when the CLI reports a readable version older than the one Suru has
/// verified its wire against.
///
/// A CLI that refuses the question is one Suru cannot vouch for either: nothing this Provider needs
/// postdates the question, so a CLI old enough not to know it is one Suru still cannot vouch for.
/// A CLI that answers nothing at all is something else: a Provider that has stopped working rather
/// than one the user updates away from, so it fails the way any unanswered request does.
async fn verify_version(
    transport: &StreamJsonTransport,
    request_timeout: Duration,
) -> Result<Option<String>, ProviderError> {
    let reported = transport
        .control_request(&ControlRequest::GetBinaryVersion, request_timeout)
        .await
        .map_err(|failure| {
            if failure.is_refusal() {
                return version_drift(format!(
                    "the Claude Code CLI could not tell Suru its version: {}",
                    failure.into_error()
                ));
            }
            claude_error_context("Claude availability probe failed", failure.into_error())
        })?;
    let reported: NativeBinaryVersion = serde_json::from_value(reported).map_err(|error| {
        version_drift(format!(
            "the Claude Code CLI reported a version Suru could not read: {error}"
        ))
    })?;
    CLAUDE_SUGGESTED_VERSION
        .warning_for(&reported.version)
        .map_err(|_| {
            version_drift(format!(
                "the Claude Code CLI reported the version `{}`, which Suru could not read",
                reported.version
            ))
        })
}

/// A CLI Suru cannot vouch for driving, as the typed condition. The user owns their own CLI, so the
/// version is theirs to fix: reported this way it reads as an actionable condition rather than as a
/// Provider that broke.
fn version_drift(message: impl AsRef<str>) -> ProviderError {
    claude_error(message).mark_unavailable(ProviderUnavailability::IncompatibleVersion)
}

/// Fails with the typed not-signed-in condition unless the CLI holds credentials it can work with
/// right now.
///
/// Authentication is entirely between the user and the Claude Code CLI — its own login, a bearer
/// token, an API key in the environment, or a third-party cloud's credentials — so Suru neither
/// asks for nor stores any of it. All a probe can do about a signed-out CLI is say so, and ask
/// again on the next refresh.
async fn verify_signed_in(
    transport: &StreamJsonTransport,
    request_timeout: Duration,
) -> Result<(), ProviderError> {
    let handshake = transport
        .control_request(&ControlRequest::Initialize, request_timeout)
        .await
        .map_err(|failure| {
            claude_error_context("Claude sign-in check failed", failure.into_error())
        })?;
    let handshake: NativeInitialize = serde_json::from_value(handshake).map_err(|error| {
        claude_error(format!(
            "Claude Code CLI returned an invalid initialize response: {error}"
        ))
    })?;
    if holds_credentials(&handshake.account) {
        return Ok(());
    }
    Err(ProviderError::unavailable(
        ProviderUnavailability::NotSignedIn,
        "sign in with the Claude Code CLI",
    ))
}

/// Whether the account the CLI reports is one it can make requests under.
///
/// A third-party backend authenticates outside the CLI altogether — AWS credentials, Google Cloud's
/// application default credentials, an enterprise gateway — and reports none of the other fields,
/// so there is nothing there for Suru to judge and the CLI is taken at its word. On an Anthropic
/// login the account names whatever the credentials are: the signed-in user, the API key's source,
/// or the bearer token's. A CLI holding none of them is one the user has to sign in to.
fn holds_credentials(account: &NativeAccount) -> bool {
    if account
        .api_provider
        .as_deref()
        .is_some_and(|backend| backend != FIRST_PARTY_BACKEND)
    {
        return true;
    }
    names_something(&account.email)
        || names_something(&account.api_key_source)
        || names_something(&account.token_source)
}

fn names_something(field: &Option<String>) -> bool {
    field
        .as_deref()
        .is_some_and(|value| !value.is_empty() && value != NO_SOURCE)
}

#[cfg(test)]
mod tests {
    use super::{NativeAccount, holds_credentials};

    fn account(fields: serde_json::Value) -> NativeAccount {
        serde_json::from_value(fields).expect("the account decodes")
    }

    #[test]
    fn an_account_naming_any_credential_is_signed_in() {
        for signed_in in [
            serde_json::json!({
                "email": "user@example.com",
                "organization": "Example",
                "subscriptionType": "Claude Max",
                "apiProvider": "firstParty",
            }),
            serde_json::json!({
                "tokenSource": "none",
                "apiKeySource": "ANTHROPIC_API_KEY",
                "apiProvider": "firstParty",
            }),
            serde_json::json!({ "tokenSource": "/login managed key" }),
            // A third-party cloud holds its credentials outside the CLI, so there is nothing here
            // for Suru to judge.
            serde_json::json!({ "apiProvider": "bedrock" }),
        ] {
            assert!(
                holds_credentials(&account(signed_in.clone())),
                "{signed_in} is an account the CLI can make requests under"
            );
        }
    }

    #[test]
    fn an_account_naming_no_credential_at_all_is_signed_out() {
        for signed_out in [
            serde_json::json!({ "tokenSource": "none", "apiProvider": "firstParty" }),
            serde_json::json!({ "email": "", "apiProvider": "firstParty" }),
            serde_json::json!({}),
        ] {
            assert!(
                !holds_credentials(&account(signed_out.clone())),
                "{signed_out} is an account no one is signed in to"
            );
        }
    }
}
