//! Copilot's three unavailability conditions: a CLI that isn't installed, one holding no
//! credentials, and one speaking a protocol version Suru does not. Each reaches the catalog as its
//! own typed reason, and each clears on the refresh after the user fixes it outside Suru. Beside
//! them, the one way a signed-in CLI must never read as unavailable: credentials it keeps to
//! itself.

use crate::support::{
    COPILOT_MODELS, ScriptedCopilot, connect_arm, copilot_catalog, gh_cli_signed_in_arm, hosting,
    models_arm, signed_in_arm, signed_out_arm, unknown_method_arm, upgradable_connect_arm,
};
use suru::{
    managed_client::ManagedClient,
    protocol::{ProviderCatalogStatus, ProviderUnavailability},
};

/// The condition the Copilot catalog reports, which every one of these tests reads the same way.
fn condition(status: &ProviderCatalogStatus) -> (ProviderUnavailability, String) {
    let ProviderCatalogStatus::Unavailable { reason, message } = status else {
        panic!("the Copilot catalog reports a typed unavailability, got {status:?}");
    };
    (*reason, message.clone())
}

/// Asserts the refresh after the user has fixed the condition outside Suru finds Copilot usable
/// again, which is the recovery every one of these conditions shares.
async fn assert_the_refresh_clears_the_condition(client: &ManagedClient) {
    let refreshed = client.refresh_models().await.expect("refresh the catalog");
    let refreshed = copilot_catalog(&refreshed);
    assert_eq!(
        refreshed.status,
        ProviderCatalogStatus::Fresh,
        "the refresh re-checks the condition and finds it fixed, with no restart in between"
    );
    assert!(
        !refreshed.models.is_empty(),
        "the Provider that was unavailable now serves its Models"
    );
}

#[tokio::test]
async fn a_copilot_cli_that_is_not_installed_is_reported_as_such_until_it_is_installed() {
    let copilot = ScriptedCopilot::with_models(COPILOT_MODELS);
    copilot.uninstall();
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&copilot, "copilot-not-installed", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let (reason, message) = condition(&copilot_catalog(&catalog).status);
    assert_eq!(reason, ProviderUnavailability::NotInstalled);
    assert!(
        message.contains("copilot"),
        "the condition names the CLI Suru could not launch, got: {message}"
    );
    assert_eq!(
        copilot.launches(),
        0,
        "a CLI that isn't there is answered without a process"
    );

    copilot.install();
    assert_the_refresh_clears_the_condition(&client).await;

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn a_cli_holding_no_credentials_is_reported_as_not_signed_in_until_the_user_signs_in() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        connect_arm(),
        signed_out_arm(),
        models_arm(COPILOT_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&copilot, "copilot-not-signed-in", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let (reason, message) = condition(&copilot_catalog(&catalog).status);
    assert_eq!(reason, ProviderUnavailability::NotSignedIn);
    assert!(
        message.contains("sign in"),
        "the condition says what the user does about it, got: {message}"
    );
    assert_eq!(
        copilot.methods(),
        ["connect", "status.get", "account.getCurrentAuth"],
        "the sign-in state is asked of the CLI at discovery time, \
         and a signed-out CLI is never asked for Models"
    );

    copilot.sign_in();
    assert_the_refresh_clears_the_condition(&client).await;
    assert_eq!(
        copilot.launches(),
        1,
        "a signed-out CLI is still a live process, so the recovery reuses it"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn a_cli_signed_in_through_the_gh_cli_serves_its_models_without_showing_suru_a_token() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        connect_arm(),
        gh_cli_signed_in_arm(),
        models_arm(COPILOT_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&copilot, "copilot-gh-cli-sign-in", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("the catalog request itself is answered");
    let catalog = copilot_catalog(&catalog);
    assert_eq!(
        catalog.status,
        ProviderCatalogStatus::Fresh,
        "credentials the CLI keeps to itself are still credentials on file, \
         so the sign-in check passes without ever reading a token"
    );
    assert!(
        !catalog.models.is_empty(),
        "a CLI signed in through the gh CLI serves its Models like any other"
    );
    assert_eq!(
        copilot.methods(),
        [
            "connect",
            "status.get",
            "account.getCurrentAuth",
            "models.list"
        ],
        "the sign-in check reads the CLI's answer and goes on to the catalog"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn a_cli_speaking_a_protocol_suru_does_not_is_reported_as_an_incompatible_version() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        upgradable_connect_arm(),
        signed_in_arm(),
        models_arm(COPILOT_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&copilot, "copilot-version-drift", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("a protocol mismatch is an answer, not a crash");
    let (reason, message) = condition(&copilot_catalog(&catalog).status);
    assert_eq!(reason, ProviderUnavailability::IncompatibleVersion);
    assert!(
        message.contains("version"),
        "the condition names the version drift, got: {message}"
    );

    copilot.upgrade();
    assert_the_refresh_clears_the_condition(&client).await;
    assert_eq!(
        copilot.launches(),
        2,
        "a handshake that fails stops its process, so the recovery launches a fresh one"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn a_cli_too_old_for_the_sign_in_query_is_reported_as_an_incompatible_version() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        connect_arm(),
        unknown_method_arm(),
        models_arm(COPILOT_MODELS),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (server, client) = hosting(&copilot, "copilot-legacy-cli", state_dir.path()).await;

    let catalog = client
        .list_models()
        .await
        .expect("a CLI that predates the query is an answer, not a crash");
    let (reason, _) = condition(&copilot_catalog(&catalog).status);
    assert_eq!(
        reason,
        ProviderUnavailability::IncompatibleVersion,
        "a CLI too old to answer the sign-in query reads as version drift the handshake let \
         through, rather than as a Provider that broke"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}
