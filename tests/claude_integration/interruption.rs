//! Interrupting a Claude Turn: the background work the agent spawned is stopped first, then the
//! loop itself, and the Turn Settles as interrupted on the result that follows.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    CLAUDE_MODELS, LiveTurn, ScriptedClaude, after_probe, agent_messages, discovery_arms,
    interrupt_arm, silent_interrupt_arm, stop_task_arm, user_turn_arm,
};
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, InitialPrompt, MessageStatus, PromptDelivery,
        PromptId, PromptStatus, SessionStatus, TurnStatus,
    },
    provider::ClaudeRuntime,
};
use tokio::time::{Duration, timeout};

/// A Turn that runs a command in the background, streams an answer, and leaves both running. The
/// CLI reports the background work twice — the roster it replaces on every membership change, and
/// the task's own start — the way a live 2.1.237 CLI does.
const WORK_IN_FLIGHT: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"sleep 600\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"task-live","task_type":"local_bash","description":"sleep 600"}],"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"task-live","tool_use_id":"toolu_1","description":"sleep 600","is_backgrounded":true,"task_type":"local_bash","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Halfway"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

/// What the CLI writes once the interrupt has stopped its loop: the interruption on the record as a
/// user message, then the terminal result whose `terminal_reason` says the Turn was aborted rather
/// than failed — the errored subtype notwithstanding, as a live 2.1.280 CLI reports it.
const ABORTED_RESULT: &str = r#"      emit '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":11,"num_turns":1,"errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],"terminal_reason":"aborted_streaming","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn an_interrupt_stops_the_background_work_first_and_settles_the_turn_as_interrupted() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(WORK_IN_FLIGHT),
        stop_task_arm(),
        interrupt_arm(ABORTED_RESULT),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-interruption",
        "Work until I stop you",
    )
    .await;
    // The interrupt must find the background work already reported, because what it stops first is
    // whatever the CLI has said is running.
    live.wait_for(
        "the background command reaches the Transcript",
        |snapshot| !snapshot.activities.is_empty() && !agent_messages(snapshot).is_empty(),
    )
    .await;

    live.client
        .interrupt_session(live.session_id)
        .await
        .expect("Claude acknowledges the interrupt");

    let interrupted = live
        .wait_for("the interrupted Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;

    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(interrupted.session.status, SessionStatus::Idle);
    let [message] = agent_messages(&interrupted)[..] else {
        panic!(
            "the Message the Turn was streaming survives it, got {:?}",
            interrupted.messages
        );
    };
    assert_eq!(message.content, "Halfway");
    assert_eq!(
        message.status,
        MessageStatus::Completed,
        "an interrupted Turn leaves no Message streaming"
    );
    let [
        Activity::Command {
            status, command, ..
        },
    ] = interrupted.activities.as_slice()
    else {
        panic!(
            "the abandoned command settles beside the Turn, got {:?}",
            interrupted.activities
        );
    };
    assert_eq!(command, "sleep 600");
    assert_eq!(
        *status,
        ActivityStatus::Interrupted,
        "a command the interrupt cut off settles interrupted with the Turn"
    );

    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "stop_task", "interrupt"]),
        "the background work is stopped before the loop is, because an interrupt alone \
         leaves it running"
    );
    let request_named = |subtype: &str| {
        claude
            .requests()
            .into_iter()
            .find(|request| request.pointer("/request/subtype") == Some(&subtype.into()))
            .unwrap_or_else(|| panic!("the interrupt issues a `{subtype}` control request"))
    };
    let stopped = request_named("stop_task");
    assert_eq!(
        stopped.pointer("/request/task_id"),
        Some(&"task-live".into()),
        "the stop names the live task the CLI reported: {stopped}"
    );
    let interrupt = request_named("interrupt");
    assert_eq!(
        interrupt.pointer("/request/cancel_queued"),
        Some(&true.into()),
        "stopping a Turn stops what the user queued into it too: {interrupt}"
    );

    live.shutdown().await;
}

/// The same Turn without any background work behind it.
const NOTHING_IN_THE_BACKGROUND: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Halfway"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

#[tokio::test]
async fn an_interrupt_with_no_background_work_asks_the_cli_to_stop_nothing() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(NOTHING_IN_THE_BACKGROUND),
        stop_task_arm(),
        interrupt_arm(ABORTED_RESULT),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-interruption-idle-background",
        "Work until I stop you",
    )
    .await;
    live.wait_for("the answer starts streaming", |snapshot| {
        !agent_messages(snapshot).is_empty()
    })
    .await;

    live.client
        .interrupt_session(live.session_id)
        .await
        .expect("Claude acknowledges the interrupt");
    let interrupted = live
        .wait_for("the interrupted Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;

    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "interrupt"]),
        "a Turn with nothing running in the background is stopped by the interrupt alone"
    );

    live.shutdown().await;
}

/// A Turn that calls a Tool whose result never comes back, then starts thinking and is stopped
/// mid-thought.
const TOOL_CALL_AND_THINKING_IN_FLIGHT: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_search","name":"mcp__github__search_issues","input":{"query":"fold"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"","signature":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"**Weighing it up**\n\nStill going"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

#[tokio::test]
async fn an_interrupt_settles_the_tool_call_and_thinking_it_cut_off_as_interrupted() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(TOOL_CALL_AND_THINKING_IN_FLIGHT),
        stop_task_arm(),
        interrupt_arm(ABORTED_RESULT),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-interruption-tool-call",
        "Work until I stop you",
    )
    .await;
    live.wait_for(
        "the Tool Call and thinking reach the Transcript",
        |snapshot| snapshot.activities.len() == 2,
    )
    .await;

    live.client
        .interrupt_session(live.session_id)
        .await
        .expect("Claude acknowledges the interrupt");
    let interrupted = live
        .wait_for("the interrupted Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;

    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    let [
        Activity::ToolCall {
            status: tool_call, ..
        },
        Activity::Reasoning {
            status: reasoning,
            title,
            ..
        },
    ] = interrupted.activities.as_slice()
    else {
        panic!(
            "the Tool Call and thinking settle beside the Turn, got {:?}",
            interrupted.activities
        );
    };
    assert_eq!(
        *tool_call,
        ActivityStatus::Interrupted,
        "a Tool Call the interrupt cut off settles interrupted"
    );
    assert_eq!(
        *reasoning,
        ActivityStatus::Interrupted,
        "thinking the interrupt cut off settles interrupted"
    );
    assert_eq!(
        title.as_deref(),
        Some("Weighing it up"),
        "a block cut short keeps the title its split was still withholding"
    );

    live.shutdown().await;
}

/// A steered Turn the user then stops. The Prompt leaves its answer streaming; the steer behind it
/// is queued into a loop that never reaches it, and the CLI says so — `still_queued` names the
/// message that survived the interrupt, which the CLI then answers in a stretch of its own. The
/// third Prompt is the Turn the user runs afterwards.
const STEERED_THEN_STOPPED: &str = r#"      prompts=$(( ${prompts:-0} + 1 ))
      if [ "$prompts" -eq 1 ]; then
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Halfway"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      elif [ "$prompts" -eq 3 ]; then
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Starting over"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":3,"num_turns":1,"result":"Starting over","terminal_reason":"completed","session_id":"prov-session"}'
      fi
"#;

/// The interrupt lands while the loop is waiting on a tool, and the steer it never reached runs
/// afterwards as a stretch of its own — output belonging to a Turn that has already Settled. A live
/// 2.1.283 CLI answers no such steer: asked to cancel what was queued, it reports the steer
/// `cancelled` and lists it in the receipt (docs/validation/0407-claude-folded-steer.md, case F).
/// The stretch stands for a CLI that answers one anyway, and what it pins is narrow: a stretch that
/// opens no message of its own — it sends no `message_start` — joins neither the interrupted Turn
/// nor the one after it, and lands on no Turn at all. A stretch that did open one would begin a
/// native Continuation, as any loop the CLI runs after a Turn has Settled does.
const ABORTED_WITH_A_SURVIVING_STEER: &str = r#"      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":11,"num_turns":1,"errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use"],"terminal_reason":"aborted_tools","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Bonjour"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":2,"num_turns":1,"result":"Bonjour","terminal_reason":"completed","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn an_interrupted_turn_takes_its_steer_with_it_and_keeps_nothing_that_lands_after() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(STEERED_THEN_STOPPED),
        interrupt_arm(ABORTED_WITH_A_SURVIVING_STEER),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-interrupted-steer",
        "Work until I stop you",
    )
    .await;
    live.wait_for("the answer starts streaming", |snapshot| {
        !agent_messages(snapshot).is_empty()
    })
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
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer the running Turn");
    live.wait_for("the steer reaches the running loop", |snapshot| {
        snapshot
            .prompts
            .iter()
            .any(|prompt| prompt.id == steer.id && prompt.status == PromptStatus::Delivered)
    })
    .await;

    live.client
        .interrupt_session(live.session_id)
        .await
        .expect("Claude acknowledges the interrupt");
    live.wait_for("the interrupted Turn settles", |snapshot| {
        snapshot.turns[0].status != TurnStatus::Active
    })
    .await;

    // The Turn the user runs next is what makes the stretch the surviving steer produced readable:
    // its own output cannot reach the Transcript before everything written ahead of it has.
    live.client
        .admit_prompt(
            live.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Start over".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit the Prompt after the interruption");
    let afterwards = live
        .wait_for("the Turn after the interruption settles", |snapshot| {
            snapshot
                .turns
                .get(1)
                .is_some_and(|turn| turn.status != TurnStatus::Active)
        })
        .await;

    assert_eq!(afterwards.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        afterwards.turns[1].status,
        TurnStatus::Completed,
        "the Session picks up cleanly from a Turn that was steered and then stopped"
    );
    assert_eq!(
        agent_messages(&afterwards)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Halfway", "Starting over"],
        "what the CLI answered the surviving steer with lands on no Turn at all"
    );

    live.shutdown().await;
}

/// A steered Turn caught between its loops (docs/validation/0407-claude-folded-steer.md, cases D
/// and F). The Prompt's loop ends with its `result` while the steer is only `queued`, and the
/// steer has yet to begin the loop of its own that would answer it — the stand-in never starts it,
/// so a test can interrupt in that window. The interrupt then runs no loop: asked to cancel what
/// was queued, the CLI reports the steer `cancelled` and lists it in the receipt, and no aborted
/// `result` follows.
const STEER_LEFT_QUEUED_BETWEEN_LOOPS: &str = r#"      prompts=$(( ${prompts:-0} + 1 ))
      if [ "$prompts" -eq 1 ]; then
        prompt=$uuid
        lifecycle "$prompt" queued
        lifecycle "$prompt" started
        emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hello"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      else
        steer=$uuid
        lifecycle "$steer" queued
        emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":9,"num_turns":1,"result":"Hello","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":10,"output_tokens":5}}'
        lifecycle "$prompt" completed
      fi
"#;

/// The interrupt as the CLI answers it with no loop running: the queued steer is cancelled, and
/// the receipt says so.
const CANCELLING_THE_QUEUED_STEER: &str = r#"    *'"subtype":"interrupt"'*)
      lifecycle "$steer" cancelled
      printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"still_queued":[],"cancelled":["'"$steer"'"]}}}'
      ;;
"#;

#[tokio::test]
async fn an_interrupt_between_a_steered_turns_loops_settles_it_interrupted() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(STEER_LEFT_QUEUED_BETWEEN_LOOPS),
        CANCELLING_THE_QUEUED_STEER,
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-interrupt-between-loops",
        "Say hello",
    )
    .await;
    live.wait_for("the answer starts streaming", |snapshot| {
        !agent_messages(snapshot).is_empty()
    })
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
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer the running Turn");
    let between = live
        .wait_for(
            "the Prompt's loop ends while the steer waits for a loop of its own",
            |snapshot| {
                snapshot
                    .prompts
                    .iter()
                    .any(|prompt| prompt.id == steer.id && prompt.status == PromptStatus::Delivered)
                    && snapshot.turns[0].usage.is_some()
            },
        )
        .await;
    assert_eq!(
        between.turns[0].status,
        TurnStatus::Active,
        "the loop the steer never joined ended without ending the Turn"
    );

    live.client
        .interrupt_session(live.session_id)
        .await
        .expect("Claude acknowledges the interrupt");
    let stopped = live
        .wait_for("the interrupted Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;

    assert_eq!(
        stopped.turns[0].status,
        TurnStatus::Interrupted,
        "the user stopped the Turn, though no loop was running to abort"
    );
    assert_eq!(stopped.turns.len(), 1, "no Continuation began");
    assert_eq!(
        agent_messages(&stopped)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Hello"]
    );
    let interrupt = claude
        .requests()
        .into_iter()
        .find(|request| {
            request
                .pointer("/request/subtype")
                .and_then(|subtype| subtype.as_str())
                == Some("interrupt")
        })
        .expect("the interrupt reached the CLI");
    assert_eq!(
        interrupt.pointer("/request/cancel_queued"),
        Some(&serde_json::Value::Bool(true)),
        "the interrupt asks for the queued steer to be dropped with it"
    );

    live.shutdown().await;
}

#[tokio::test]
async fn an_interrupt_the_cli_never_answers_fails_the_turn_within_the_injected_timeout() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(WORK_IN_FLIGHT),
        stop_task_arm(),
        silent_interrupt_arm(),
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable())
            .with_interrupt_request_timeout(Duration::from_millis(300)),
        "claude-interrupt-timeout",
        "Work until I stop you",
    )
    .await;
    live.wait_for(
        "the background command reaches the Transcript",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;

    let error = timeout(
        PROGRESS_DEADLINE,
        live.client.interrupt_session(live.session_id),
    )
    .await
    .expect("an interrupt the CLI never answers gives up on its own")
    .expect_err("the unanswered interrupt reaches the client as a failure");
    assert!(
        error.to_string().contains("timed out handling `interrupt`"),
        "the failure says the CLI never answered, got: {error:#}"
    );

    let failed = live
        .wait_for(
            "the Turn its interrupt could not stop settles",
            |snapshot| snapshot.turns[0].status != TurnStatus::Active,
        )
        .await;

    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.session.status, SessionStatus::Idle);
    let Some(Activity::Error { text, .. }) = failed
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Error { .. }))
    else {
        panic!(
            "the interruption failure reads without opening the Log, got {:?}",
            failed.activities
        );
    };
    assert!(
        text.contains("timed out handling `interrupt`"),
        "the Error Activity says what went wrong, got: {text}"
    );

    live.shutdown().await;
}
