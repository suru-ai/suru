//! A Copilot Session surviving a Suru restart: the Copilot Session identifier persists as the Suru
//! Session's Resume State, and the Session that reopens carries on through Copilot's native resume
//! rather than opening a new Copilot Session.

use std::{path::Path, sync::Arc};

use crate::support::{
    ScriptedCopilot, agent_messages, connect_in, conversation_arms, forgotten_session_arm,
    resumable_conversation_fixture, send_arm, settled_session_on,
};
use serde_json::Value;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionSubscription},
    protocol::{
        AdmitPromptRequest, CreateSessionRequest, InitialPrompt, PromptDelivery, PromptId,
        SessionId, SessionSnapshot, TurnStatus, Workspace,
    },
    provider::CopilotRuntime,
    server::{self, RunningServer, ServerConfig},
};

/// One agent Message and the loop going idle — enough for a Turn to settle either side of a
/// restart.
const STREAMED_MESSAGE: &str = r#"      event e1 assistant.message_start '{"messageId":"m1"}'
      event e2 assistant.message_delta '{"messageId":"m1","deltaContent":"Still here"}'
      event e3 assistant.message '{"messageId":"m1","content":"Still here"}'
      event e4 session.idle '{}'
"#;

/// A CLI that creates a Session but no longer holds it when Suru comes back for it.
fn forgetful_fixture() -> ScriptedCopilot {
    ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        forgotten_session_arm(),
        send_arm(STREAMED_MESSAGE),
    ))
}

/// A Suru Session established on one server and reopened on its replacement, holding everything the
/// reopened Session runs on for as long as the test does — the state and data directories and the
/// Workspace included, which are only alive while this is.
struct RestartedSession {
    _state_dir: tempfile::TempDir,
    _data_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    server: RunningServer,
    client: ManagedClient,
    feed: SessionSubscription,
    session_id: SessionId,
}

impl RestartedSession {
    /// Opens a Session on `copilot` under `channel`, runs its first Turn to completion, stops the
    /// server, and comes back subscribed to the same Session on a replacement server. `channel` is
    /// the client channel, so each test needs its own.
    async fn establish(copilot: &ScriptedCopilot, channel: &'static str) -> Self {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let config = ServerConfig::new(state_dir.path(), channel)
            .expect("configure server")
            .with_data_dir(data_dir.path());

        let original = spawn(config.clone(), copilot).await;
        let client = connect_in(client_config(state_dir.path(), data_dir.path(), channel)).await;
        let created = client
            .create_session(CreateSessionRequest {
                agent_selection: None,
                workspace: Workspace {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Establish durable Copilot context".to_owned(),
                    skill_invocations: Vec::new(),
                },
            })
            .await
            .expect("create original Session");
        let session_id = created.session.id;
        let mut feed = client
            .subscribe_session(session_id)
            .await
            .expect("subscribe to original Session");
        let settled = settled_session_on(&client, &mut feed, session_id, 0).await;
        assert_eq!(settled.turns[0].status, TurnStatus::Completed);
        drop(feed);
        drop(client);
        original.shutdown().await.expect("stop original server");
        // The CLI the Session was created on is gone before the replacement asks for it back, so
        // only a process the replacement spawned itself can serve the resume.
        copilot.wait_for_exit().await;

        let server = spawn(config, copilot).await;
        let client = connect_in(client_config(state_dir.path(), data_dir.path(), channel)).await;
        let feed = client
            .subscribe_session(session_id)
            .await
            .expect("subscribe to reopened Session");
        Self {
            _state_dir: state_dir,
            _data_dir: data_dir,
            _workspace: workspace,
            server,
            client,
            feed,
            session_id,
        }
    }

    /// Delivers `prompt` to the reopened Session and comes back once the Turn it began settles.
    async fn continue_with(&mut self, prompt: &str) -> SessionSnapshot {
        self.client
            .admit_prompt(
                self.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: prompt.to_owned(),
                        skill_invocations: Vec::new(),
                    },
                    delivery: PromptDelivery::Steer,
                },
            )
            .await
            .expect("admit Prompt to reopened Session");
        settled_session_on(&self.client, &mut self.feed, self.session_id, 1).await
    }

    async fn shutdown(self) {
        let Self {
            server,
            client,
            feed,
            ..
        } = self;
        drop(feed);
        drop(client);
        server.shutdown().await.expect("stop replacement server");
    }
}

async fn spawn(config: ServerConfig, copilot: &ScriptedCopilot) -> RunningServer {
    server::spawn_with_provider(config, Arc::new(CopilotRuntime::new(copilot.executable())))
        .await
        .expect("spawn server")
}

fn client_config(state_dir: &Path, data_dir: &Path, channel: &str) -> ManagedClientConfig {
    ManagedClientConfig::new(state_dir, channel)
        .expect("configure client")
        .with_data_dir(data_dir)
}

#[tokio::test]
async fn a_reopened_session_resumes_its_persisted_copilot_session_after_a_restart() {
    let copilot = resumable_conversation_fixture(STREAMED_MESSAGE);
    let mut restarted =
        RestartedSession::establish(&copilot, "copilot-persisted-resume-state").await;

    let settled = restarted
        .continue_with("Continue durable Copilot context")
        .await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Completed,
        "the Turn after the restart runs on the resumed Copilot Session: {:?}",
        settled.activities
    );
    assert_eq!(
        agent_messages(&settled).len(),
        2,
        "the restored Transcript keeps its first agent Message and gains the second"
    );

    assert_eq!(
        copilot.launches(),
        2,
        "the resume demand spawns a harness process of its own"
    );
    assert_eq!(
        copilot
            .methods()
            .into_iter()
            .filter(|method| method.starts_with("session.") || method == "connect")
            .collect::<Vec<_>>(),
        [
            "connect",
            "session.create",
            "session.model.getCurrent",
            "session.send",
            "connect",
            "session.resume",
            "session.skills.reload",
            "session.model.getCurrent",
            "session.send",
        ],
        "the replacement server handshakes a fresh process and resumes rather than creating"
    );
    let requests = copilot.requests();
    assert_eq!(
        session_parameter(&requests, "session.create"),
        session_parameter(&requests, "session.resume"),
        "the resume names the Copilot Session the Suru Session was created under"
    );

    restarted.shutdown().await;
}

#[tokio::test]
async fn a_session_copilot_can_no_longer_resume_stays_viewable_and_says_why() {
    let copilot = forgetful_fixture();
    let mut restarted =
        RestartedSession::establish(&copilot, "copilot-unusable-resume-state").await;

    let settled = restarted
        .continue_with("Continue a Session Copilot has lost")
        .await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Failed,
        "a Session Copilot cannot resume fails to continue rather than starting over"
    );
    assert_eq!(
        agent_messages(&settled).len(),
        1,
        "the Transcript the Session was restored with stays readable"
    );
    let failure = settled
        .activities
        .iter()
        .map(|activity| format!("{activity:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        failure.contains("Copilot Session resume failed"),
        "the failure names the resume as what went wrong: {failure}"
    );
    assert_eq!(
        copilot
            .methods()
            .iter()
            .filter(|method| *method == "session.send")
            .count(),
        1,
        "no Prompt reaches a Copilot Session that never resumed"
    );

    restarted.shutdown().await;
}

/// The `sessionId` the first `method` request carried.
fn session_parameter(requests: &[Value], method: &str) -> String {
    requests
        .iter()
        .find(|request| request["method"] == method)
        .and_then(|request| request["params"]["sessionId"].as_str())
        .unwrap_or_else(|| panic!("the scripted Copilot received a {method} naming a Session"))
        .to_owned()
}
