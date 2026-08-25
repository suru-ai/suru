//! Steering a Copilot Turn that is already under way: a Prompt delivered into the running Turn
//! rather than one that begins another.

use crate::support::{
    LiveTurn, ScriptedCopilot, agent_messages, conversation_arms, conversation_fixture,
    settled_session_on,
};
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, InitialPrompt, MessageRole, PromptDelivery, PromptId,
        PromptStatus, TurnStatus,
    },
    provider::CopilotRuntime,
};

/// A Turn that holds until the test releases it, so a steer arrives while its loop is still
/// running. The wait runs in the background, because the read loop it is holding is also what
/// takes delivery of the steer.
const HELD_TURN: &str = r#"      sends=$(( ${sends:-0} + 1 ))
      if [ "$sends" -eq 1 ]; then
        (
          while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
          event e1 assistant.message '{"messageId":"m1","content":"Bonjour"}'
          event e2 session.idle '{}'
        ) &
      fi
"#;

/// A Copilot that takes the Prompt that begins a Turn but refuses the one steering it.
const REFUSED_STEER: &str = r#"    *'"method":"session.send"'*)
      case "$body" in
        *'"mode":"immediate"'*)
          reply '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32600,"message":"fixture refused the steer"}}'
          ;;
        *)
          reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"messageId":"fixture-message"}}'
          ;;
      esac
      ;;
"#;

#[tokio::test]
async fn a_steer_prompt_joins_the_running_turn_rather_than_beginning_another() {
    let copilot = conversation_fixture(HELD_TURN);
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-steering",
        "Say hello",
    )
    .await;

    let steer = live
        .client
        .admit_prompt(
            live.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Answer in French instead".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer the running Turn");
    assert_eq!(steer.status, PromptStatus::Pending);

    let steered = live
        .wait_for("the steer Prompt reaches the running Turn", |snapshot| {
            snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == steer.id && prompt.status == PromptStatus::Delivered)
        })
        .await;

    assert_eq!(
        steered.turns.len(),
        1,
        "a steer joins the Turn in flight rather than beginning another"
    );
    assert_eq!(steered.turns[0].status, TurnStatus::Active);
    let asked = steered
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .collect::<Vec<_>>();
    assert_eq!(
        asked
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Say hello", "Answer in French instead"]
    );
    assert_eq!(
        asked[1].turn_id, live.turn_id,
        "the steer's Message belongs to the Turn it steered"
    );

    let sends = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.send")
        .collect::<Vec<_>>();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1]["params"]["prompt"], "Answer in French instead");
    assert_eq!(
        sends[1]["params"]["mode"], "immediate",
        "a steer is injected into the running loop rather than queued behind it"
    );
    assert!(
        sends[0]["params"].get("mode").is_none(),
        "the Prompt that began the Turn is delivered as Copilot delivers one by default: {}",
        sends[0]["params"]
    );

    copilot.release();
    let settled = settled_session_on(&live.client, &mut live.feed, live.session_id, 0).await;
    assert_eq!(settled.turns.len(), 1, "no second Turn ever began");
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&settled)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Bonjour"],
        "the steered Turn answers once, under the redirection"
    );

    live.shutdown().await;
}

#[tokio::test]
async fn a_refused_steer_leaves_its_prompt_pending_and_says_what_went_wrong() {
    let copilot = ScriptedCopilot::new(&format!("{}{REFUSED_STEER}", conversation_arms()));
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-steer-refusal",
        "Start something long",
    )
    .await;

    let steer = live
        .client
        .admit_prompt(
            live.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Change course".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer the running Turn");

    let refused = live
        .wait_for("the refused steer reaches the Session feed", |snapshot| {
            !snapshot.activities.is_empty()
        })
        .await;

    let [Activity::Error { text, .. }] = refused.activities.as_slice() else {
        panic!(
            "a refused steer says what went wrong, got {:?}",
            refused.activities
        );
    };
    assert!(
        text.contains("fixture refused the steer"),
        "the refusal carries what Copilot said, got: {text}"
    );
    assert_eq!(
        refused
            .prompts
            .iter()
            .find(|prompt| prompt.id == steer.id)
            .expect("the steer Prompt remains authoritative")
            .status,
        PromptStatus::Pending,
        "a steer Copilot never took is not delivered"
    );
    assert_eq!(refused.turns.len(), 1);
    assert_eq!(
        refused.turns[0].status,
        TurnStatus::Active,
        "the Turn the steer missed keeps running"
    );
    assert_eq!(
        refused
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1,
        "a steer Copilot never took leaves no Message behind"
    );

    live.shutdown().await;
}
