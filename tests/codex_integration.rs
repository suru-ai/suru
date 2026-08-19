#![cfg(unix)]

#[allow(dead_code)]
mod support;

use std::{os::unix::fs::PermissionsExt, sync::Arc};

use chidori::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, SessionEvent, SessionSubscription,
    },
    protocol::{
        Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, AgentSelection,
        CreateSessionRequest, FileChange, InitialPrompt, MessageRole, MessageStatus,
        ModelAvailability, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionKind,
        ModelOptionRole, ModelOptionSelection, ModelOptionValue, PromptDelivery, PromptId,
        PromptStatus, ProviderCatalogStatus, ProviderId, SessionChange, SessionId, SessionSnapshot,
        SessionStatus, ShutdownReason, TranscriptItem, TurnId, TurnStatus, Workspace,
    },
    provider::CodexRuntime,
    server::{self, RunningServer, ServerConfig},
    tui::{Application, ApplicationEvent},
};
use serde_json::Value;
use sysinfo::{Pid, System};
use tokio::time::{Duration, timeout};

use support::request_server_shutdown;

const SCRIPTED_CODEX: &str = r#"#!/bin/sh
if [ "$1" != "app-server" ]; then
  exit 64
fi

i=0
while [ "$i" -lt 5000 ]; do
  printf 'fixture diagnostic output that must stay off stdout\n' >&2
  i=$((i + 1))
done

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":999,"result":{"ignored":"uncorrelated response"}}'
      printf '%s' '{"id":"1","result":{"userAgent":"fixture","futureField":true'
      printf '%s\n' '}}'
      ;;
    *'"method":"initialized"'*)
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread","futureField":true},"model":"gpt-fixture","modelProvider":"fixture","futureField":true}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":"3","result":{"turn":{"id":"native-turn","status":"inProgress","futureField":true}}}'
      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
        sleep 0.01
      done
      printf '%s\n' '{"method":"future/notification","params":{"ignored":true}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"futureItem","id":"ignored-item","payload":{"unknown":true}}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"futureItem","id":"ignored-item","payload":{"unknown":true}}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"other-thread","turn":{"id":"other-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"other-thread","turnId":"other-turn","item":{"type":"agentMessage","id":"other-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"other-thread","turnId":"other-turn","itemId":"other-message","delta":"wrong Session content"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"other-thread","turnId":"other-turn","item":{"type":"agentMessage","id":"other-message","text":"wrong Session content"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"native-command","command":"cargo test --test codex_integration","cwd":"/fixture/work","status":"inProgress","futureField":true},"futureField":true}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-command","delta":"wrong command output"}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-command","delta":"running ","futureField":true}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-command","delta":"tests\n"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"native-command","command":"cargo test --test codex_integration","cwd":"/fixture/work","status":"completed","aggregatedOutput":"running tests\nall green\n","exitCode":0,"futureField":true},"futureField":true}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"fileChange","id":"native-file-change","changes":[{"path":"src/protocol.rs","kind":{"type":"update","movePath":null},"diff":"private start patch"}],"status":"inProgress","futureField":{"opaque":true}},"futureField":true}}'
      printf '%s\n' '{"method":"item/fileChange/patchUpdated","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-file-change","changes":[{"path":"wrong-session.txt","kind":{"type":"add"},"diff":"wrong item patch"}]}}'
      printf '%s\n' '{"method":"item/fileChange/patchUpdated","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-file-change","changes":[{"path":"src/protocol.rs","kind":{"type":"update","movePath":"src/protocol_v2.rs"},"diff":"private updated patch"},{"path":"tests/session_protocol.rs","kind":{"type":"add"},"diff":"private added patch"}],"futureField":true}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"fileChange","id":"native-file-change","changes":[{"path":"src/protocol.rs","kind":{"type":"update","movePath":"src/protocol_v2.rs"},"diff":"private final patch"},{"path":"tests/session_protocol.rs","kind":{"type":"add"},"diff":"private final test patch"},{"path":"obsolete.txt","kind":{"type":"delete"},"diff":"private deleted patch"}],"status":"completed","futureField":{"native":true}},"futureField":true}}'
      printf '%s' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"native-message","text":"","futureField":true},"futureField":true'
      printf '%s\n' '}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-message","delta":"wrong item content"}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-message","delta":"Hello","futureField":true}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-message","delta":" from Codex"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"native-message","text":"Hello from Codex"},"futureField":true}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[],"futureField":true},"futureField":true}}'
      ;;
  esac
done
"#;

const MODEL_CATALOG_CODEX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$CODEX_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$CODEX_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$CODEX_FIXTURE_ATTEMPTS"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"model/list"'*'"cursor":null'*)
      if [ "$attempt" -gt 1 ]; then
        printf '%s\n' '{"id":2,"error":{"code":-32001,"message":"temporary catalog outage"}}'
      else
        printf '%s\n' '{"id":2,"result":{"data":[{"id":"gpt-opaque","displayName":"GPT Fixture","description":"Primary fixture model","hidden":false,"supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Faster"},{"reasoningEffort":"xhigh","description":"Deepest"}],"defaultReasoningEffort":"xhigh","serviceTiers":[{"id":"flex-native","name":"Flex","description":"Flexible processing"},{"id":"fast-native","name":"Fast","description":"Priority processing"}],"defaultServiceTier":"flex-native","isDefault":true},{"id":"hidden-model","displayName":"Hidden","description":"Not selectable","hidden":true,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":false}],"nextCursor":"opaque-page-2"}}'
      fi
      ;;
    *'"method":"model/list"'*'"cursor":"opaque-page-2"'*)
      printf '%s\n' '{"id":3,"result":{"data":[{"id":"fast-model","displayName":"Fast Fixture","description":"Has independent speed","hidden":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[{"id":"fast","name":"Fast","description":"Priority processing"}],"defaultServiceTier":null,"isDefault":false}],"nextCursor":null}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-opaque","reasoningEffort":"xhigh","serviceTier":"flex-native"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

const MALFORMED_MODEL_CATALOG_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"model/list"'*)
      printf '%s\n' '{"id":2,"result":{"data":[{"id":"incomplete"}],"nextCursor":null}}'
      ;;
  esac
done
"#;

const INITIALIZE_REJECTION: &str = r#"#!/bin/sh
read -r line
printf '%s\n' '{"id":"1","error":{"code":-32000,"message":"fixture rejected initialization"}}'
"#;

const MALFORMED_OUTPUT: &str = r#"#!/bin/sh
read -r line
printf '%s\n' '{this is not JSON'
"#;

const EOF_WITH_PENDING_REQUEST: &str = r#"#!/bin/sh
read -r line
exit 0
"#;

const TURN_REQUEST_ERROR: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":"2","result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"error":{"code":-32001,"message":"fixture rejected Turn startup"}}'
      ;;
  esac
done
"#;

const SELECTED_MODEL_REJECTION: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"failed","error":{"message":"selection rejected by fixture","codexErrorInfo":"badRequest","additionalDetails":"{\"error\":{\"param\":\"model\"}}"},"items":[]}}}'
      ;;
  esac
done
"#;

const SELECTED_OPTION_REJECTION: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"failed","error":{"message":"selected service tier is unavailable","codexErrorInfo":"badRequest","additionalDetails":"{\"error\":{\"param\":\"serviceTier\"}}"},"items":[]}}}'
      ;;
  esac
done
"#;

const NON_MODEL_BAD_REQUEST: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"failed","error":{"message":"fixture rejected the input","codexErrorInfo":"badRequest","additionalDetails":"{\"error\":{\"param\":\"input\"}}"},"items":[]}}}'
      ;;
  esac
done
"#;

const SELECTED_MODEL_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"provider-default"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"thread/settings/updated","params":{"threadId":"native-thread","threadSettings":{"model":"effective-model","effort":"low","serviceTier":"flex"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

const THREAD_DEFAULT_OPTIONS_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"provider-default","reasoningEffort":"medium","serviceTier":null}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"thread/settings/updated","params":{"threadId":"native-thread","threadSettings":{"model":"provider-default","serviceTier":"fast"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

const EOF_AFTER_TURN_START: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      exit 0
      ;;
  esac
done
"#;

const START_TURN: &str = r#"      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'"#;

const SEQUENTIAL_QUEUE_TURNS: &str = r#"      turn_index=$((turn_index + 1))
      response_id=$((turn_index + 2))
      printf '{"id":%s,"result":{"turn":{"id":"native-turn-%s"}}}\n' "$response_id" "$turn_index"
      (
        while [ ! -e "$CODEX_FIXTURE_RELEASE-$turn_index" ]; do
          sleep 0.01
        done
        printf '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-%s","status":"completed","items":[]}}}\n' "$turn_index"
        printf '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-%s","status":"completed","items":[]}}}\n' "$turn_index"
      ) &"#;

const TERMINAL_BOUNDARY_TURNS: &str = r#"      turn_index=$((turn_index + 1))
      if [ "$turn_index" -eq 1 ]; then
        while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
          sleep 0.01
        done
        printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn-1"}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-1","status":"completed","items":[]}}}'
      else
        printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn-2"}}}'
        (
          while [ ! -e "$CODEX_FIXTURE_RELEASE-2" ]; do
            sleep 0.01
          done
          printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-2","status":"completed","items":[]}}}'
        ) &
      fi"#;

const UNEXPECTED_STEER: &str = "      exit 65";

const NONZERO_AFTER_TURN_START: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      exit 17
      ;;
  esac
done
"#;

const UNKNOWN_SERVER_REQUEST: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":"unknown-correlation","method":"future/request","params":{"ignored":true}}'
      read -r response
      printf '%s\n' "$response" >> "$CODEX_FIXTURE_LOG"
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

const UNSUPPORTED_SERVER_REQUEST: &str = r#"#!/bin/sh
request_pipe="$CODEX_FIXTURE_LOG.pipe"
mkfifo "$request_pipe"
exec 3<&0
while IFS= read -r captured; do
  printf '%s\n' "$captured" >> "$CODEX_FIXTURE_LOG"
  printf '%s\n' "$captured"
done <&3 > "$request_pipe" &
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"id":"unsupported-correlation","method":"$CODEX_FIXTURE_METHOD","params":{"fixture":true}}'
      read -r response
      while :; do sleep 1; done
      ;;
  esac
done < "$request_pipe"
"#;

const PROCESS_LOSS_WITH_UNSUPPORTED_REQUEST: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"id":"abandoned-interaction","method":"item/tool/requestUserInput","params":{"fixture":true}}'
      exit 17
      ;;
  esac
done
"#;

const MULTIPROCESS_SCRIPT_PREFIX: &str = r#"#!/bin/sh
attempt=1
if [ -e "$CODEX_FIXTURE_ATTEMPTS" ]; then
  attempt=$(( $(cat "$CODEX_FIXTURE_ATTEMPTS") + 1 ))
fi
printf '%s\n' "$attempt" > "$CODEX_FIXTURE_ATTEMPTS"

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
"#;

const STARTUP_RETRY: &str = r#"
    *'"method":"initialize"'*)
      if [ "$attempt" -eq 1 ]; then
        printf '%s\n' '{"id":1,"error":{"code":-32000,"message":"fixture startup failed once"}}'
        exit 0
      fi
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"retry-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"retry-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"retry-thread","turn":{"id":"retry-turn","status":"completed","items":[]}}}'
      ;;
"#;

const ACTIVE_PROCESS_LOSS_THEN_RESUME: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"recoverable-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"recoverable-thread","turns":[{"id":"native-turn-1"}]},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      if [ "$attempt" -eq 1 ]; then
        exit 17
      fi
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn-2"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"recoverable-thread","turnId":"native-turn-2","item":{"type":"agentMessage","id":"resumed-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"recoverable-thread","turnId":"native-turn-2","itemId":"resumed-message","delta":"Recovered context"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"recoverable-thread","turnId":"native-turn-2","item":{"type":"agentMessage","id":"resumed-message","text":"Recovered context"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"recoverable-thread","turn":{"id":"native-turn-2","status":"completed","items":[]}}}'
      ;;
"#;

const ACTIVE_PROCESS_LOSS_THEN_RESUME_REJECTION: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"rejected-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"error":{"code":-32001,"message":"fixture cannot resume this Thread"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn-1"}}}'
      exit 17
      ;;
"#;

const IDLE_PROCESS_LOSS_THEN_RESUME: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"idle-loss-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"idle-loss-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      if [ "$attempt" -eq 1 ]; then
        printf '%s\n' '{"id":3,"result":{"turn":{"id":"idle-turn-1"}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"idle-loss-thread","turn":{"id":"idle-turn-1","status":"completed","items":[]}}}'
        printf '%s\n' 'exited' > "$CODEX_FIXTURE_EXITED"
        exit 17
      fi
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"idle-turn-2"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"idle-loss-thread","turn":{"id":"idle-turn-2","status":"completed","items":[]}}}'
      ;;
"#;

const INTERRUPTION_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      ;;
    *'"method":"turn/interrupt"'*)
__INTERRUPT_ACTION__
      ;;
  esac
done
"#;

const ACKNOWLEDGE_AND_COMPLETE_INTERRUPTION: &str = r#"      printf '%s\n' '{"id":4,"result":{}}'
      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
        sleep 0.01
      done
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"trailing-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"trailing-message","delta":"Trailing output"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"trailing-message","text":"Trailing output"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'"#;

const REJECT_INTERRUPTION: &str = r#"      printf '%s\n' '{"id":4,"error":{"code":-32600,"message":"fixture rejected interruption"}}'"#;
const TIME_OUT_INTERRUPTION: &str = "      sleep 10";
const LOSE_PROCESS_DURING_INTERRUPT: &str = "      exit 23";

const PROMPT_OPERATION_CODEX: &str = r#"#!/bin/sh
turn_index=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
__TURN_START_ACTION__
      ;;
    *'"method":"turn/steer"'*)
__STEER_ACTION__
      ;;
  esac
done
wait
"#;

const COOPERATIVE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
sleep 30 &
printf '%s\n' "$!" > "$CODEX_FIXTURE_CHILD_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"working-message","text":""}}}'
      printf ready > "$CODEX_FIXTURE_READY"
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"late-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"late-message","delta":"late shutdown output"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"late-message","text":"late shutdown output"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      printf '%s\n' '{"id":4,"result":{}}'
      ;;
  esac
done
"#;

const PENDING_TURN_START_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf ready > "$CODEX_FIXTURE_READY"
      (
        while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
          sleep 0.01
        done
        printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      ) &
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":4,"result":{}}'
      ;;
  esac
done
"#;

const UNRESPONSIVE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
sleep 30 &
printf '%s\n' "$!" > "$CODEX_FIXTURE_CHILD_PID"

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"working-message","text":""}}}'
      printf ready > "$CODEX_FIXTURE_READY"
      ;;
    *'"method":"turn/interrupt"'*)
      while :; do :; done
      ;;
  esac
done
"#;

const ACCEPT_STEER: &str = r#"      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
        sleep 0.01
      done
      printf '%s\n' '{"id":4,"result":{"turnId":"native-turn"}}'"#;

const REJECT_STEER: &str = r#"      printf '%s\n' '{"id":4,"error":{"code":-32600,"message":"fixture rejected steering"}}'"#;

const COMPLETE_THEN_ACCEPT_STEER: &str = r#"      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"id":4,"result":{"turnId":"native-turn"}}'"#;

const COMPLETE_THEN_REJECT_STEER: &str = r#"      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"id":4,"error":{"code":-32600,"message":"no active turn to steer"}}'"#;

const LOSE_PROCESS_DURING_STEER: &str = "      exit 29";

fn interruption_script(action: &str) -> String {
    INTERRUPTION_CODEX.replace("__INTERRUPT_ACTION__", action)
}

fn steering_script(action: &str) -> String {
    prompt_operation_script(START_TURN, action)
}

fn prompt_operation_script(start_action: &str, steer_action: &str) -> String {
    PROMPT_OPERATION_CODEX
        .replace("__TURN_START_ACTION__", start_action)
        .replace("__STEER_ACTION__", steer_action)
}

struct SteeringFixture {
    codex: ScriptedCodex,
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    server: RunningServer,
    client: ManagedClient,
    feed: SessionSubscription,
    session_id: SessionId,
    turn_id: TurnId,
}

impl SteeringFixture {
    async fn start(script: &str, channel: &str) -> Self {
        let codex = ScriptedCodex::new(script);
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let server = server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            Arc::new(CodexRuntime::new(codex.executable())),
        )
        .await
        .expect("spawn server");
        let mut client = ManagedClient::connect(
            ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
        )
        .await
        .expect("connect client");
        receive_initial_state(&mut client).await;
        let created = client
            .create_session(CreateSessionRequest {
                agent_selection: None,
                workspace: Workspace {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Begin the steering fixture".to_owned(),
                },
            })
            .await
            .expect("create Session");
        codex.wait_for_method("turn/start").await;
        let active = timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = client
                    .read_session(created.session.id)
                    .await
                    .expect("read Session");
                if snapshot
                    .turns
                    .first()
                    .is_some_and(|turn| turn.status == TurnStatus::Active)
                {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial Codex Turn becomes active");
        let mut feed = client
            .subscribe_session(created.session.id)
            .await
            .expect("subscribe to Session SSE");
        assert!(matches!(
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session snapshot is valid"),
            SessionEvent::Snapshot(_)
        ));
        Self {
            codex,
            _state_dir: state_dir,
            _workspace: workspace,
            server,
            client,
            feed,
            session_id: created.session.id,
            turn_id: active.turns[0].id,
        }
    }

    async fn wait_for(
        &mut self,
        description: &str,
        predicate: impl Fn(&SessionSnapshot) -> bool,
    ) -> SessionSnapshot {
        timeout(Duration::from_secs(2), async {
            loop {
                self.feed
                    .next()
                    .await
                    .expect("Session feed remains open")
                    .expect("Session update is valid");
                let snapshot = self
                    .client
                    .read_session(self.session_id)
                    .await
                    .expect("read steering fixture Session");
                if predicate(&snapshot) {
                    return snapshot;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{description}"))
    }

    async fn shutdown(self) {
        let Self {
            codex,
            _state_dir: state_dir,
            _workspace: workspace,
            server,
            client,
            feed,
            ..
        } = self;
        drop(feed);
        drop(client);
        server.shutdown().await.expect("shut down server");
        drop(codex);
        drop(state_dir);
        drop(workspace);
    }
}

enum TerminalSteerOutcome {
    Accepted,
    Rejected { error: &'static str },
}

#[tokio::test]
async fn codex_model_catalog_is_paginated_normalized_and_kept_across_refresh_failure() {
    let codex = ScriptedCodex::new(MODEL_CATALOG_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-model-catalog").expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-model-catalog")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let initial = client.list_models().await.expect("discover Codex Models");
    assert_eq!(initial.providers.len(), 1);
    let catalog = &initial.providers[0];
    assert_eq!(catalog.provider, ProviderId::new("codex"));
    assert_eq!(catalog.status, ProviderCatalogStatus::Fresh);
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(catalog.models[0].id, ModelId::new("gpt-opaque"));
    assert_eq!(catalog.models[0].display_name, "GPT Fixture");
    assert_eq!(catalog.models[0].availability, ModelAvailability::Available);
    assert_eq!(catalog.models[0].options.len(), 2);
    assert_eq!(
        catalog.models[0].options[0].role,
        ModelOptionRole::ReasoningEffort
    );
    let ModelOptionKind::Select { choices, default } = &catalog.models[0].options[0].kind else {
        panic!("reasoning effort is a Select option");
    };
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        ["low", "xhigh"]
    );
    assert_eq!(default.as_str(), "xhigh");
    assert_eq!(catalog.models[0].options[1].role, ModelOptionRole::Speed);
    let ModelOptionKind::Select { choices, default } = &catalog.models[0].options[1].kind else {
        panic!("speed is a Select option");
    };
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        ["flex-native", "fast-native"]
    );
    assert_eq!(default.as_str(), "flex-native");
    assert_eq!(
        catalog.models[0].default_agent_selection().options,
        vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("xhigh"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("flex-native"),
                },
            },
        ]
    );

    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let incomplete = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("gpt-opaque"),
                options: vec![ModelOptionSelection {
                    id: ModelOptionId::new("reasoning_effort"),
                    value: ModelOptionValue::Select {
                        choice: ModelOptionChoiceId::new("low"),
                    },
                }],
            }),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reject incomplete advertised options".to_owned(),
            },
        })
        .await
        .expect_err("reject an incomplete advertised Agent Selection");
    assert!(incomplete.to_string().contains("service_tier"));

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Materialize every advertised default".to_owned(),
            },
        })
        .await
        .expect("create Session from cached advertised defaults");
    assert_eq!(
        created.session.agent_selection,
        Some(catalog.models[0].default_agent_selection())
    );
    assert_eq!(catalog.models[1].options[0].role, ModelOptionRole::Speed);
    let ModelOptionKind::Select { choices, default } = &catalog.models[1].options[0].kind else {
        panic!("speed is a Select option");
    };
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.id.as_str())
            .collect::<Vec<_>>(),
        ["default", "fast"]
    );
    assert_eq!(default.as_str(), "default");

    let cached = client.list_models().await.expect("read cached catalog");
    assert_eq!(
        cached.providers[0].status,
        ProviderCatalogStatus::Refreshing
    );
    assert_eq!(cached.providers[0].models, catalog.models);
    let stale = client
        .refresh_models()
        .await
        .expect("observe failed background refresh");
    assert_eq!(stale.providers[0].models, catalog.models);
    assert!(matches!(
        &stale.providers[0].status,
        ProviderCatalogStatus::Stale { message } if message.contains("temporary catalog outage")
    ));

    let requests = codex.requests();
    assert!(requests.iter().any(|request| {
        request.get("method").and_then(Value::as_str) == Some("model/list")
            && request["params"]["cursor"] == Value::String("opaque-page-2".to_owned())
    }));
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn malformed_codex_model_results_are_reported_for_the_codex_provider() {
    let codex = ScriptedCodex::new(MALFORMED_MODEL_CATALOG_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-malformed-model-catalog")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-malformed-model-catalog")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let catalog = client.list_models().await.expect("request Model catalog");
    assert_eq!(catalog.providers[0].provider, ProviderId::new("codex"));
    assert!(catalog.providers[0].models.is_empty());
    assert!(matches!(
        &catalog.providers[0].status,
        ProviderCatalogStatus::Failed { message }
            if message.contains("invalid model/list response")
    ));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn scripted_codex_delivers_the_authoritative_queue_once_in_admission_order() {
    let codex = ScriptedCodex::new(&prompt_operation_script(
        SEQUENTIAL_QUEUE_TURNS,
        UNEXPECTED_STEER,
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-scripted-queueing").expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut author = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-queueing")
            .expect("configure author client"),
    )
    .await
    .expect("connect author client");
    let mut observer = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-queueing")
            .expect("configure observer client"),
    )
    .await
    .expect("connect observer client");
    receive_initial_state(&mut author).await;
    receive_initial_state(&mut observer).await;

    let created = author
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Run the initial Turn".to_owned(),
            },
        })
        .await
        .expect("create queued-delivery Session");
    let session_id = created.session.id;
    let mut feed = observer
        .subscribe_session(session_id)
        .await
        .expect("observe Session through SSE");
    codex.wait_for_method_count("turn/start", 1).await;

    let first_request = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Run the first queued Turn".to_owned(),
        },
        delivery: PromptDelivery::Queue,
    };
    let cancelled_request = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Cancel this queued Turn".to_owned(),
        },
        delivery: PromptDelivery::Queue,
    };
    let second_request = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Run the second queued Turn".to_owned(),
        },
        delivery: PromptDelivery::Queue,
    };
    let first = author
        .admit_prompt(session_id, first_request.clone())
        .await
        .expect("admit first queued Prompt");
    let cancelled = author
        .admit_prompt(session_id, cancelled_request)
        .await
        .expect("admit cancellable queued Prompt");
    let second = author
        .admit_prompt(session_id, second_request)
        .await
        .expect("admit second queued Prompt");
    assert_eq!(first.status, PromptStatus::Pending);
    assert_eq!(cancelled.status, PromptStatus::Pending);
    assert_eq!(second.status, PromptStatus::Pending);
    assert!(first.admission_order < cancelled.admission_order);
    assert!(cancelled.admission_order < second.admission_order);

    let cancelled = observer
        .cancel_prompt(session_id, cancelled.id)
        .await
        .expect("cancel queued Prompt from another client");
    assert_eq!(cancelled.status, PromptStatus::Cancelled);
    let retried = author
        .admit_prompt(session_id, first_request.clone())
        .await
        .expect("retry identical queued admission");
    assert_eq!(retried.status, PromptStatus::Pending);

    let pending = observer
        .read_session(session_id)
        .await
        .expect("read pending queue from observer");
    assert_eq!(pending.session.status, SessionStatus::Active);
    assert_eq!(pending.turns.len(), 1);
    assert_eq!(pending.messages.len(), 1);
    assert_eq!(
        pending
            .prompts
            .iter()
            .map(|prompt| prompt.status)
            .collect::<Vec<_>>(),
        [
            PromptStatus::Delivered,
            PromptStatus::Pending,
            PromptStatus::Cancelled,
            PromptStatus::Pending,
        ]
    );
    assert_eq!(
        codex
            .requests()
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .count(),
        1
    );

    codex.release_turn(1);
    codex.wait_for_method_count("turn/start", 2).await;
    let first_queued_turn = wait_for_session_snapshot(
        &observer,
        &mut feed,
        session_id,
        "first queued Prompt begins after the first terminal boundary",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Active,
    )
    .await;
    assert_eq!(first_queued_turn.session.status, SessionStatus::Active);
    assert_eq!(first_queued_turn.turns[0].status, TurnStatus::Completed);
    assert_eq!(first_queued_turn.turns[1].prompt_id, first.id);
    assert_eq!(first_queued_turn.messages.len(), 2);
    assert_eq!(first_queued_turn.messages[1].content, first.text);
    assert_eq!(
        first_queued_turn
            .prompts
            .iter()
            .find(|prompt| prompt.id == second.id)
            .expect("second queued Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );

    let delivered_retry = author
        .admit_prompt(session_id, first_request)
        .await
        .expect("retry delivered queued admission");
    assert_eq!(delivered_retry.status, PromptStatus::Delivered);

    codex.release_turn(2);
    codex.wait_for_method_count("turn/start", 3).await;
    let second_queued_turn = wait_for_session_snapshot(
        &observer,
        &mut feed,
        session_id,
        "second queued Prompt begins after the second terminal boundary",
        |snapshot| snapshot.turns.len() == 3 && snapshot.turns[2].status == TurnStatus::Active,
    )
    .await;
    assert_eq!(second_queued_turn.session.status, SessionStatus::Active);
    assert_eq!(second_queued_turn.turns[1].status, TurnStatus::Completed);
    assert_eq!(second_queued_turn.turns[2].prompt_id, second.id);
    assert_eq!(second_queued_turn.messages.len(), 3);
    assert_eq!(second_queued_turn.messages[2].content, second.text);

    codex.release_turn(3);
    let completed = wait_for_session_snapshot(
        &observer,
        &mut feed,
        session_id,
        "the final queued Turn reaches idle",
        |snapshot| snapshot.turns[2].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.turns.len(), 3);
    assert_eq!(completed.messages.len(), 3);
    assert_eq!(
        completed
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        [
            "Run the initial Turn",
            "Run the first queued Turn",
            "Run the second queued Turn",
        ]
    );
    let requests = codex.requests();
    let turn_starts = requests
        .iter()
        .filter(|request| request["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 3);
    assert_eq!(
        turn_starts
            .iter()
            .map(|request| request["params"]["input"][0]["text"]
                .as_str()
                .expect("turn/start contains text input"))
            .collect::<Vec<_>>(),
        [
            "Run the initial Turn",
            "Run the first queued Turn",
            "Run the second queued Turn",
        ]
    );
    assert!(
        requests
            .iter()
            .all(|request| request["method"] != "turn/queue"),
        "Chidori must not submit Prompts to Codex's queue API"
    );

    drop(feed);
    drop(observer);
    drop(author);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn scripted_codex_accepts_one_idempotent_steer_before_delivering_its_prompt() {
    let mut fixture =
        SteeringFixture::start(&steering_script(ACCEPT_STEER), "codex-scripted-steering").await;
    let active_turn_id = fixture.turn_id;
    let prompt_id = PromptId::new();
    let steer = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: prompt_id,
            text: "Change course".to_owned(),
        },
        delivery: PromptDelivery::Steer,
    };

    let admitted = fixture
        .client
        .admit_prompt(fixture.session_id, steer.clone())
        .await
        .expect("admit steer Prompt");
    assert_eq!(admitted.status, PromptStatus::Pending);
    fixture.codex.wait_for_method("turn/steer").await;

    let retried = fixture
        .client
        .admit_prompt(fixture.session_id, steer.clone())
        .await
        .expect("retry identical steer admission");
    assert_eq!(retried.status, PromptStatus::Pending);
    let before_acknowledgement = fixture
        .client
        .read_session(fixture.session_id)
        .await
        .expect("read Session before steering acknowledgement");
    assert_eq!(
        before_acknowledgement.prompts[1].status,
        PromptStatus::Pending
    );
    assert_eq!(before_acknowledgement.turns.len(), 1);
    assert_eq!(before_acknowledgement.messages.len(), 1);

    fixture.codex.release();
    let delivered = fixture
        .wait_for("accepted steer becomes delivered", |snapshot| {
            snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == prompt_id)
                .is_some_and(|prompt| prompt.status == PromptStatus::Delivered)
        })
        .await;
    assert_eq!(delivered.turns.len(), 1);
    assert_eq!(delivered.turns[0].id, active_turn_id);
    assert_eq!(delivered.turns[0].status, TurnStatus::Active);
    assert_eq!(delivered.messages.len(), 2);
    assert_eq!(delivered.messages[1].role, MessageRole::User);
    assert_eq!(delivered.messages[1].turn_id, active_turn_id);
    assert_eq!(delivered.messages[1].content, "Change course");

    let after_delivery_retry = fixture
        .client
        .admit_prompt(fixture.session_id, steer)
        .await
        .expect("retry delivered steer admission");
    assert_eq!(after_delivery_retry.status, PromptStatus::Delivered);
    let steer_requests = fixture
        .codex
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/steer")
        .collect::<Vec<_>>();
    assert_eq!(steer_requests.len(), 1);
    assert_eq!(steer_requests[0]["params"]["threadId"], "native-thread");
    assert_eq!(steer_requests[0]["params"]["expectedTurnId"], "native-turn");
    assert_eq!(
        steer_requests[0]["params"]["input"],
        serde_json::json!([{ "type": "text", "text": "Change course" }])
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn scripted_codex_rejection_keeps_the_steer_pending_and_reports_the_failure() {
    let mut fixture = SteeringFixture::start(
        &steering_script(REJECT_STEER),
        "codex-scripted-steer-rejection",
    )
    .await;
    let prompt_id = PromptId::new();
    let admitted = fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Try a rejected course correction".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer Prompt");
    assert_eq!(admitted.status, PromptStatus::Pending);

    let rejected = fixture
        .wait_for("steering rejection reaches Session SSE", |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(activity,
                    Activity::Error { text, .. } if text.contains("fixture rejected steering"))
            })
        })
        .await;
    assert_eq!(
        rejected
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .expect("steer Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(rejected.turns.len(), 1);
    assert_eq!(rejected.turns[0].status, TurnStatus::Active);
    assert_eq!(rejected.messages.len(), 1);
    assert_eq!(rejected.activities.len(), 1);
    assert!(matches!(rejected.activities[0], Activity::Error { .. }));

    fixture.shutdown().await;
}

#[tokio::test]
async fn scripted_codex_transport_loss_during_steering_keeps_the_prompt_pending() {
    let mut fixture = SteeringFixture::start(
        &steering_script(LOSE_PROCESS_DURING_STEER),
        "codex-scripted-steer-process-loss",
    )
    .await;
    let prompt_id = PromptId::new();
    fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Steer across a broken transport".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer Prompt");

    let failed = fixture
        .wait_for("steering transport loss reaches Session SSE", |snapshot| {
            snapshot.turns[0].status == TurnStatus::Failed
        })
        .await;
    assert_eq!(
        failed
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .expect("steer Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(failed.messages.len(), 1);
    assert!(
        failed.activities.iter().any(|activity| matches!(activity,
                Activity::Error { text, .. } if text.contains("status: 29"))),
        "transport failure is visible: {:?}",
        failed.activities
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn scripted_codex_terminal_completion_during_steering_is_ordered_after_its_response() {
    assert_terminal_steering_race(
        COMPLETE_THEN_ACCEPT_STEER,
        "codex-steer-terminal-acceptance",
        TerminalSteerOutcome::Accepted,
    )
    .await;
    assert_terminal_steering_race(
        COMPLETE_THEN_REJECT_STEER,
        "codex-steer-terminal-rejection",
        TerminalSteerOutcome::Rejected {
            error: "no active turn to steer",
        },
    )
    .await;
}

#[tokio::test]
async fn scripted_codex_handles_pending_steers_before_starting_the_queued_turn() {
    let mut fixture = SteeringFixture::start(
        &prompt_operation_script(TERMINAL_BOUNDARY_TURNS, UNEXPECTED_STEER),
        "codex-terminal-steer-priority",
    )
    .await;
    let queued = fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Run after the terminal boundary".to_owned(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit queued Prompt before the boundary steer");
    let steer = fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Apply this steer before continuing".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer while native completion is pending");
    assert!(queued.admission_order < steer.admission_order);
    assert_eq!(queued.status, PromptStatus::Pending);
    assert_eq!(steer.status, PromptStatus::Pending);

    fixture.codex.release();
    fixture.codex.wait_for_method_count("turn/start", 2).await;

    let continued = fixture
        .wait_for(
            "steer is reconciled before the queued Turn begins",
            |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Active,
        )
        .await;
    assert_eq!(continued.session.status, SessionStatus::Active);
    assert_eq!(continued.turns[0].status, TurnStatus::Completed);
    assert_eq!(continued.turns[1].prompt_id, queued.id);
    assert_eq!(continued.messages.len(), 3);
    assert_eq!(continued.messages[0].content, "Begin the steering fixture");
    assert_eq!(continued.messages[1].content, steer.text);
    assert_eq!(continued.messages[1].turn_id, continued.turns[0].id);
    assert_eq!(continued.messages[2].content, queued.text);
    assert_eq!(continued.messages[2].turn_id, continued.turns[1].id);
    assert_eq!(
        continued
            .prompts
            .iter()
            .find(|prompt| prompt.id == steer.id)
            .expect("boundary steer remains authoritative")
            .status,
        PromptStatus::Delivered
    );
    assert_eq!(
        fixture
            .codex
            .requests()
            .iter()
            .filter(|request| request["method"] == "turn/steer")
            .count(),
        0
    );
    let turn_starts = fixture
        .codex
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 2);
    assert_eq!(
        turn_starts[1]["params"]["input"],
        serde_json::json!([{ "type": "text", "text": "Run after the terminal boundary" }])
    );

    fixture.codex.release_turn(2);
    let completed = fixture
        .wait_for("queued Turn reaches its terminal boundary", |snapshot| {
            snapshot.turns[1].status == TurnStatus::Completed
        })
        .await;
    assert_eq!(completed.session.status, SessionStatus::Idle);

    fixture.shutdown().await;
}

async fn assert_terminal_steering_race(
    steer_action: &str,
    channel: &str,
    outcome: TerminalSteerOutcome,
) {
    let mut fixture = SteeringFixture::start(&steering_script(steer_action), channel).await;
    let active_turn_id = fixture.turn_id;
    let prompt_id = PromptId::new();
    fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Race the terminal boundary".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit boundary steer Prompt");

    let completed = fixture
        .wait_for(
            "terminal steering race settles through Session SSE",
            |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
        )
        .await;
    let expected_error = match outcome {
        TerminalSteerOutcome::Accepted => None,
        TerminalSteerOutcome::Rejected { error } => Some(error),
    };
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.turns.len(), 1);
    assert_eq!(completed.turns[0].id, active_turn_id);
    assert_eq!(
        completed
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .expect("boundary steer Prompt remains authoritative")
            .status,
        PromptStatus::Delivered
    );
    assert_eq!(completed.messages.len(), 2);
    assert_eq!(
        completed
            .messages
            .iter()
            .filter(|message| message.content == "Race the terminal boundary")
            .count(),
        1
    );
    match expected_error {
        Some(expected_error) => assert!(
            completed
                .activities
                .iter()
                .any(|activity| matches!(activity,
                    Activity::Error { text, .. } if text.contains(expected_error)))
        ),
        None => assert!(completed.activities.is_empty()),
    }

    fixture.shutdown().await;
}
const PENDING_INITIALIZE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
done
"#;

#[tokio::test]
async fn selected_model_is_lowered_to_codex_and_effective_model_is_projected_back() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-selected-model").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-selected-model")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let requested = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("requested-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("fast"),
                },
            },
        ],
    };
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(requested),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use the requested native Model".to_owned(),
            },
        })
        .await
        .expect("create selected Codex Session");
    fixture.wait_for_method("turn/start").await;
    let turn_start = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/start")
        .expect("capture native turn/start");
    assert_eq!(turn_start["params"]["model"], "requested-model");
    assert_eq!(turn_start["params"]["effort"], "high");
    assert_eq!(turn_start["params"]["serviceTier"], "fast");

    let completed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read selected Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("effective Codex Model is projected");
    let effective = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("effective-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("flex"),
                },
            },
        ],
    };
    assert_eq!(completed.session.agent_selection, Some(effective.clone()));
    assert_eq!(
        completed.turns[0]
            .agent
            .as_ref()
            .map(|agent| &agent.selection),
        Some(&effective)
    );
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("requested-model") && text.contains("effective-model")
    )));
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("reasoning_effort")
                && text.contains("high")
                && text.contains("low")
    )));
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("service_tier")
                && text.contains("fast")
                && text.contains("flex")
    )));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_materializes_thread_options_and_handles_native_omission_and_clear() {
    let fixture = ScriptedCodex::new(THREAD_DEFAULT_OPTIONS_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-thread-default-options")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-thread-default-options")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use every effective default".to_owned(),
            },
        })
        .await
        .expect("create default Codex Session");
    let completed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read default Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("default Codex Turn completes");
    let expected = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("provider-default"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("medium"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("fast"),
                },
            },
        ],
    };
    assert_eq!(completed.session.agent_selection, Some(expected.clone()));
    assert_eq!(
        completed.turns[0]
            .agent
            .as_ref()
            .map(|agent| &agent.selection),
        Some(&expected)
    );
    let turn_start = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/start")
        .expect("capture default turn/start");
    assert_eq!(turn_start["params"]["effort"], "medium");
    assert!(
        turn_start["params"]
            .as_object()
            .is_some_and(|params| params.get("serviceTier") == Some(&Value::Null))
    );
    assert!(completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. }
            if text.contains("service_tier")
                && text.contains("default")
                && text.contains("fast")
    )));
    assert!(!completed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Status { text, .. } if text.contains("reasoning_effort")
    )));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_adapter_distinguishes_explicit_option_defaults_from_native_omission() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-option-defaults").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-option-defaults")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    client
        .create_session(CreateSessionRequest {
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("requested-model"),
                options: vec![
                    ModelOptionSelection {
                        id: ModelOptionId::new("reasoning_effort"),
                        value: ModelOptionValue::Select {
                            choice: ModelOptionChoiceId::new("medium"),
                        },
                    },
                    ModelOptionSelection {
                        id: ModelOptionId::new("service_tier"),
                        value: ModelOptionValue::Select {
                            choice: ModelOptionChoiceId::new("default"),
                        },
                    },
                ],
            }),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Apply explicit defaults".to_owned(),
            },
        })
        .await
        .expect("create Session with explicit defaults");
    fixture.wait_for_method_count("turn/start", 1).await;
    let explicit = fixture
        .requests()
        .into_iter()
        .find(|request| {
            request["method"] == "turn/start"
                && request["params"]["input"][0]["text"] == "Apply explicit defaults"
        })
        .expect("capture explicit-default turn/start");
    assert_eq!(explicit["params"]["effort"], "medium");
    assert!(
        explicit["params"]
            .as_object()
            .is_some_and(|params| { params.get("serviceTier") == Some(&Value::Null) })
    );

    client
        .create_session(CreateSessionRequest {
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("requested-model"),
                options: Vec::new(),
            }),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Leave options omitted".to_owned(),
            },
        })
        .await
        .expect("create Session without advertised options");
    fixture.wait_for_method_count("turn/start", 2).await;
    let omitted = fixture
        .requests()
        .into_iter()
        .find(|request| {
            request["method"] == "turn/start"
                && request["params"]["input"][0]["text"] == "Leave options omitted"
        })
        .expect("capture omitted-options turn/start");
    let omitted = omitted["params"]
        .as_object()
        .expect("turn/start params are an object");
    assert!(!omitted.contains_key("effort"));
    assert!(!omitted.contains_key("serviceTier"));

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_lowers_every_advertised_effort_and_tier_combination_independently() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-option-combinations").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-option-combinations")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    for effort in ["low", "xhigh"] {
        for service_tier in ["flex-native", "fast-native"] {
            client
                .create_session(CreateSessionRequest {
                    agent_selection: Some(AgentSelection {
                        provider: ProviderId::new("codex"),
                        model: ModelId::new("gpt-opaque"),
                        options: vec![
                            ModelOptionSelection {
                                id: ModelOptionId::new("reasoning_effort"),
                                value: ModelOptionValue::Select {
                                    choice: ModelOptionChoiceId::new(effort),
                                },
                            },
                            ModelOptionSelection {
                                id: ModelOptionId::new("service_tier"),
                                value: ModelOptionValue::Select {
                                    choice: ModelOptionChoiceId::new(service_tier),
                                },
                            },
                        ],
                    }),
                    workspace: Workspace {
                        path: workspace.path().to_owned(),
                    },
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: format!("Use {effort} with {service_tier}"),
                    },
                })
                .await
                .expect("create Session for an advertised option combination");
        }
    }
    fixture.wait_for_method_count("turn/start", 4).await;
    let mut combinations = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .map(|request| {
            (
                request["params"]["effort"]
                    .as_str()
                    .expect("effort is a string")
                    .to_owned(),
                request["params"]["serviceTier"]
                    .as_str()
                    .expect("service tier is a string")
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    combinations.sort();
    assert_eq!(
        combinations,
        [
            ("low".to_owned(), "fast-native".to_owned()),
            ("low".to_owned(), "flex-native".to_owned()),
            ("xhigh".to_owned(), "fast-native".to_owned()),
            ("xhigh".to_owned(), "flex-native".to_owned()),
        ]
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_model_rejection_never_falls_back_and_restores_the_prompt() {
    let fixture = ScriptedCodex::new(SELECTED_MODEL_REJECTION);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-selected-model-rejection")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-selected-model-rejection")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("rejected-model"),
        options: Vec::new(),
    };
    let prompt_id = PromptId::new();
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(selection.clone()),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Do not silently fall back".to_owned(),
            },
        })
        .await
        .expect("create selected Codex Session");
    let failed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read rejected Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Codex rejection becomes visible");
    let turn_starts = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 1);
    assert_eq!(turn_starts[0]["params"]["model"], "rejected-model");
    assert_eq!(failed.session.agent_selection, Some(selection));
    assert_eq!(
        failed.session.agent_selection_availability,
        ModelAvailability::Unavailable
    );
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert!(failed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text.contains("selection rejected by fixture")
    )));
    assert_eq!(failed.prompts.len(), 2);
    assert_eq!(failed.prompts[0].id, prompt_id);
    assert_ne!(failed.prompts[1].id, prompt_id);
    assert_eq!(failed.prompts[1].text, "Do not silently fall back");
    assert_eq!(failed.prompts[1].status, PromptStatus::Pending);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn codex_option_rejection_never_falls_back_and_restores_the_prompt() {
    let fixture = ScriptedCodex::new(SELECTED_OPTION_REJECTION);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-selected-option-rejection")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-selected-option-rejection")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("valid-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("service_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("retired-tier"),
                },
            },
        ],
    };
    let prompt_id = PromptId::new();
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(selection.clone()),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Do not silently replace the selected speed".to_owned(),
            },
        })
        .await
        .expect("create selected Codex Session");
    let failed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read rejected Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Codex option rejection becomes visible");
    let turn_start = fixture
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/start")
        .expect("capture rejected turn/start");
    assert_eq!(turn_start["params"]["model"], "valid-model");
    assert_eq!(turn_start["params"]["effort"], "high");
    assert_eq!(turn_start["params"]["serviceTier"], "retired-tier");
    assert_eq!(failed.session.agent_selection, Some(selection.clone()));
    assert_eq!(
        failed.session.agent_selection_availability,
        ModelAvailability::Unavailable
    );
    assert_eq!(
        failed.turns[0].agent.as_ref().map(|agent| &agent.selection),
        Some(&selection)
    );
    assert!(failed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { text, .. } if text.contains("service tier is unavailable")
    )));
    assert_eq!(failed.prompts.len(), 2);
    assert_eq!(failed.prompts[0].id, prompt_id);
    assert_ne!(failed.prompts[1].id, prompt_id);
    assert_eq!(
        failed.prompts[1].text,
        "Do not silently replace the selected speed"
    );
    assert_eq!(failed.prompts[1].status, PromptStatus::Pending);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn generic_codex_turn_rejection_does_not_mark_the_model_unavailable() {
    let fixture = ScriptedCodex::new(NON_MODEL_BAD_REQUEST);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-generic-turn-rejection")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-generic-turn-rejection")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("codex"),
                model: ModelId::new("valid-model"),
                options: Vec::new(),
            }),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Fail for a reason unrelated to Model selection".to_owned(),
            },
        })
        .await
        .expect("create selected Codex Session");
    let failed = timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read failed Codex Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generic rejection becomes visible");

    assert_eq!(
        failed.session.agent_selection_availability,
        ModelAvailability::Available
    );
    assert_eq!(failed.prompts.len(), 1);

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn scripted_codex_runs_initial_prompt_through_stdio_and_session_sse() {
    let fixture = ScriptedCodex::new(SCRIPTED_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-scripted-success").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-success")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the native harness".to_owned(),
            },
        })
        .await
        .expect("create Session without waiting for Codex startup");
    assert_eq!(created.session.agent_selection, None);
    assert_eq!(created.session.status, SessionStatus::Idle);
    assert_eq!(created.prompts[0].status, PromptStatus::Pending);
    assert!(created.turns.is_empty());

    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    let initial_event = timeout(Duration::from_secs(2), feed.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");
    assert!(matches!(initial_event, SessionEvent::Snapshot(_)));
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Session(initial_event))
        .expect("initial SSE snapshot hydrates the client projection");
    fixture.wait_for_method("turn/start").await;
    fixture.release();

    let mut streamed_command_id: Option<ActivityId> = None;
    let mut streamed_command_output = String::new();
    let mut saw_command_completion = false;
    let mut streamed_file_change_id: Option<ActivityId> = None;
    let mut streamed_file_changes = Vec::new();
    let mut file_change_update_count = 0;
    let mut saw_file_change_completion = false;
    timeout(Duration::from_secs(2), async {
        loop {
            let event = feed
                .next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            let mut turn_completed = false;
            if let SessionEvent::Updated(update) = &event {
                for change in &update.changes {
                    match change {
                        SessionChange::ActivityAdded {
                            activity:
                                Activity::Command {
                                    id,
                                    status: ActivityStatus::Active,
                                    ..
                                },
                        } => {
                            assert!(
                                streamed_command_id.replace(*id).is_none(),
                                "SSE must add exactly one command Activity"
                            );
                        }
                        SessionChange::CommandOutputAppended {
                            activity_id,
                            content,
                        } => {
                            assert_eq!(Some(*activity_id), streamed_command_id);
                            streamed_command_output.push_str(content);
                        }
                        SessionChange::CommandStatusChanged {
                            activity_id,
                            status: ActivityStatus::Completed,
                            exit_status: Some(0),
                        } => {
                            assert_eq!(Some(*activity_id), streamed_command_id);
                            saw_command_completion = true;
                        }
                        SessionChange::ActivityAdded {
                            activity:
                                Activity::FileChange {
                                    id,
                                    status: ActivityStatus::Active,
                                    changes,
                                    ..
                                },
                        } => {
                            assert!(
                                streamed_file_change_id.replace(*id).is_none(),
                                "SSE must add exactly one file-change Activity"
                            );
                            streamed_file_changes.clone_from(changes);
                        }
                        SessionChange::FileChangeUpdated {
                            activity_id,
                            changes,
                        } => {
                            assert_eq!(Some(*activity_id), streamed_file_change_id);
                            streamed_file_changes.clone_from(changes);
                            file_change_update_count += 1;
                        }
                        SessionChange::FileChangeStatusChanged {
                            activity_id,
                            status: ActivityStatus::Completed,
                        } => {
                            assert_eq!(Some(*activity_id), streamed_file_change_id);
                            saw_file_change_completion = true;
                        }
                        SessionChange::TurnStatusChanged {
                            status: TurnStatus::Completed,
                            ..
                        } => turn_completed = true,
                        _ => {}
                    }
                }
            }
            application
                .handle_event(ApplicationEvent::Session(event))
                .expect("SSE update applies through the client projection");
            if turn_completed {
                break;
            }
        }
    })
    .await
    .expect("Codex Turn reaches a terminal Session state");
    assert!(streamed_command_id.is_some());
    assert_eq!(streamed_command_output, "running tests\nall green\n");
    assert!(saw_command_completion);
    assert!(streamed_file_change_id.is_some());
    assert_eq!(file_change_update_count, 2);
    assert_eq!(
        streamed_file_changes,
        [
            FileChange::Update {
                path: "src/protocol.rs".into(),
                moved_to: Some("src/protocol_v2.rs".into()),
            },
            FileChange::Add {
                path: "tests/session_protocol.rs".into(),
            },
            FileChange::Delete {
                path: "obsolete.txt".into(),
            },
        ]
    );
    assert!(saw_file_change_completion);

    let completed = client
        .read_session(created.session.id)
        .await
        .expect("read completed Session");
    let selection = completed
        .session
        .agent_selection
        .as_ref()
        .expect("effective Codex Agent Selection is published");
    assert_eq!(selection.provider, ProviderId::new("codex"));
    assert_eq!(selection.model, ModelId::new("gpt-fixture"));
    assert!(selection.options.is_empty());
    let identity = completed.turns[0]
        .agent
        .as_ref()
        .expect("Codex Turn captures its effective Agent");
    assert_eq!(identity.agent, AgentId::new("codex"));
    assert_eq!(&identity.selection, selection);
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    let agent_message = completed
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Agent)
        .expect("Codex Agent Message is projected");
    assert_eq!(agent_message.status, MessageStatus::Completed);
    assert_eq!(agent_message.content, "Hello from Codex");
    assert!(!agent_message.content.contains("fixture diagnostic"));
    assert_eq!(completed.activities.len(), 2);
    let Activity::Command {
        id: command_activity_id,
        status,
        command,
        cwd,
        output,
        exit_status,
        ..
    } = &completed.activities[0]
    else {
        panic!("Codex command must project as command Activity");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(command, "cargo test --test codex_integration");
    assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/fixture/work")));
    assert_eq!(output, "running tests\nall green\n");
    assert_eq!(*exit_status, Some(0));
    assert_eq!(
        completed
            .transcript
            .iter()
            .filter(|item| matches!(item,
                TranscriptItem::Activity { activity_id } if activity_id == command_activity_id))
            .count(),
        1,
        "Codex command deltas must update one transcript row"
    );
    let Activity::FileChange {
        id: file_change_activity_id,
        status: file_change_status,
        changes,
        ..
    } = &completed.activities[1]
    else {
        panic!("Codex file changes must project as file-change Activity");
    };
    assert_eq!(*file_change_status, ActivityStatus::Completed);
    assert_eq!(changes, &streamed_file_changes);
    assert_eq!(
        completed
            .transcript
            .iter()
            .filter(|item| matches!(item,
                TranscriptItem::Activity { activity_id }
                    if activity_id == file_change_activity_id))
            .count(),
        1,
        "Codex file-change updates must update one transcript row"
    );
    let persisted = serde_json::to_string(&completed).expect("encode completed Session");
    for excluded in [
        "private start patch",
        "private updated patch",
        "private final patch",
        "wrong item patch",
        "futureField",
        "opaque",
    ] {
        assert!(
            !persisted.contains(excluded),
            "Session transcript persisted excluded native content {excluded:?}"
        );
    }

    let requests = fixture.requests();
    let methods = requests
        .iter()
        .filter_map(|request| request.get("method").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        ["initialize", "initialized", "thread/start", "turn/start"]
    );
    assert_eq!(
        requests[0]["params"]["capabilities"]["experimentalApi"],
        false
    );
    assert_eq!(requests[1], serde_json::json!({ "method": "initialized" }));
    assert_eq!(
        requests[2]["params"]["cwd"],
        workspace.path().to_string_lossy().as_ref()
    );
    assert_eq!(requests[2]["params"]["approvalPolicy"], "never");
    assert_eq!(requests[2]["params"]["sandbox"], "danger-full-access");
    assert_eq!(requests[2]["params"]["ephemeral"], false);
    assert!(requests[2]["params"].get("model").is_none());
    assert_eq!(requests[3]["params"]["threadId"], "native-thread");
    assert_eq!(
        requests[3]["params"]["input"],
        serde_json::json!([{ "type": "text", "text": "Explain the native harness" }])
    );
    assert_eq!(requests[3]["params"]["model"], "gpt-fixture");

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn unknown_server_request_gets_method_not_found_without_corrupting_response_routing() {
    let fixture = ScriptedCodex::new(UNKNOWN_SERVER_REQUEST);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-unknown-server-request")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-unknown-server-request")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep routing the Codex Turn".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");

    let completed = timeout(Duration::from_secs(2), async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session event is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session after unknown server request");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status != TurnStatus::Active)
            {
                return snapshot;
            }
        }
    })
    .await
    .expect("Codex Turn settles after unknown server request");

    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    assert!(completed.activities.is_empty());
    let response = fixture
        .requests()
        .into_iter()
        .find(|message| message.get("id") == Some(&Value::String("unknown-correlation".to_owned())))
        .expect("unknown server request receives a response");
    assert_eq!(response["error"]["code"], -32601);
    assert!(response.get("result").is_none());

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn unsupported_server_interactions_are_rejected_and_fail_the_active_turn() {
    for (method, channel) in [
        (
            "item/commandExecution/requestApproval",
            "codex-unsupported-approval",
        ),
        (
            "item/tool/requestUserInput",
            "codex-unsupported-structured-input",
        ),
        (
            "mcpServer/elicitation/request",
            "codex-unsupported-elicitation",
        ),
        ("item/tool/call", "codex-unsupported-host-tool"),
    ] {
        let script = UNSUPPORTED_SERVER_REQUEST.replace("$CODEX_FIXTURE_METHOD", method);
        let fixture = ScriptedCodex::new(&script);
        assert_provider_failure(fixture.executable(), channel, method).await;

        let response = fixture
            .requests()
            .into_iter()
            .find(|message| {
                message.get("id") == Some(&Value::String("unsupported-correlation".to_owned()))
            })
            .unwrap_or_else(|| panic!("{method} receives a correlated response"));
        assert_eq!(response["error"]["code"], -32000);
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(method))
        );
        assert!(response.get("result").is_none());
    }
}

#[tokio::test]
async fn scripted_codex_interrupt_acknowledges_before_trailing_output_and_terminal_event() {
    let fixture = ScriptedCodex::new(&interruption_script(ACKNOWLEDGE_AND_COMPLETE_INTERRUPTION));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-scripted-interruption")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-interruption")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep working until interrupted".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    feed.next()
        .await
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");
    fixture.wait_for_method("turn/start").await;
    let active = client
        .read_session(created.session.id)
        .await
        .expect("read active Session");
    let turn_id = active.turns[0].id;
    assert_eq!(active.turns[0].status, TurnStatus::Active);

    let acknowledged = client
        .interrupt_turn(created.session.id, turn_id)
        .await
        .expect("Codex acknowledges interruption");
    assert_eq!(acknowledged.status, TurnStatus::Active);
    let retried = client
        .interrupt_turn(created.session.id, turn_id)
        .await
        .expect("retry acknowledged interruption");
    assert_eq!(retried.status, TurnStatus::Active);

    let after_acknowledgement = client
        .read_session(created.session.id)
        .await
        .expect("read Session after interruption acknowledgement");
    assert_eq!(after_acknowledgement.session.status, SessionStatus::Active);
    assert_eq!(after_acknowledgement.turns[0].status, TurnStatus::Active);
    let interrupt_requests = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/interrupt")
        .collect::<Vec<_>>();
    assert_eq!(interrupt_requests.len(), 1);
    assert_eq!(interrupt_requests[0]["params"]["threadId"], "native-thread");
    assert_eq!(interrupt_requests[0]["params"]["turnId"], "native-turn");

    fixture.release();
    let mut interrupted_transitions = 0;
    let interrupted = timeout(Duration::from_secs(2), async {
        loop {
            let event = feed
                .next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            if let SessionEvent::Updated(update) = event {
                interrupted_transitions += update
                    .changes
                    .iter()
                    .filter(|change| {
                        matches!(
                            change,
                            chidori::protocol::SessionChange::TurnStatusChanged {
                                turn_id: changed_turn_id,
                                status: TurnStatus::Interrupted,
                            } if *changed_turn_id == turn_id
                        )
                    })
                    .count();
            }
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read interrupting Session");
            if interrupted_transitions == 1 {
                return snapshot;
            }
        }
    })
    .await
    .expect("Codex terminal interruption reaches Session SSE");

    assert_eq!(interrupted_transitions, 1);
    assert_eq!(interrupted.session.status, SessionStatus::Idle);
    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    let trailing = interrupted
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Agent)
        .expect("trailing Agent Message is accepted");
    assert_eq!(trailing.status, MessageStatus::Completed);
    assert_eq!(trailing.content, "Trailing output");
    assert!(
        timeout(Duration::from_millis(100), feed.next())
            .await
            .is_err(),
        "duplicate native completion must not publish a second Session transition"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn scripted_codex_interruption_failures_settle_the_command_and_session() {
    for (script, channel, expected_error) in [
        (
            REJECT_INTERRUPTION,
            "codex-interrupt-rejection",
            "fixture rejected interruption",
        ),
        (
            TIME_OUT_INTERRUPTION,
            "codex-interrupt-timeout",
            "timed out handling `turn/interrupt`",
        ),
        (
            LOSE_PROCESS_DURING_INTERRUPT,
            "codex-interrupt-process-loss",
            "status: 23",
        ),
    ] {
        assert_interruption_failure(script, channel, expected_error).await;
    }
}

#[tokio::test]
async fn scripted_codex_projects_failed_and_interrupted_terminal_outcomes() {
    let failed = SCRIPTED_CODEX
        .replace(
            "\"cwd\":\"/fixture/work\",\"status\":\"completed\"",
            "\"cwd\":\"/fixture/work\",\"status\":\"failed\"",
        )
        .replace("\"exitCode\":0", "\"exitCode\":17")
        .replace(
            "\"status\":\"completed\",\"items\":[]",
            "\"status\":\"failed\",\"error\":{\"message\":\"fixture Turn failed\"},\"items\":[]",
        );
    run_terminal_fixture(
        &failed,
        "codex-scripted-failed",
        TurnStatus::Failed,
        Some("fixture Turn failed"),
        ActivityStatus::Failed,
        Some(17),
    )
    .await;

    let interrupted = SCRIPTED_CODEX.replace(
        "\"status\":\"completed\",\"items\":[]",
        "\"status\":\"interrupted\",\"items\":[]",
    );
    run_terminal_fixture(
        &interrupted,
        "codex-scripted-interrupted",
        TurnStatus::Interrupted,
        None,
        ActivityStatus::Completed,
        Some(0),
    )
    .await;
}

#[tokio::test]
async fn scripted_codex_uses_completed_agent_text_when_no_deltas_arrive() {
    let without_deltas = SCRIPTED_CODEX
        .replace(
            "      printf '%s\\n' '{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"native-thread\",\"turnId\":\"native-turn\",\"itemId\":\"native-message\",\"delta\":\"Hello\",\"futureField\":true}}'\n",
            "",
        )
        .replace(
            "      printf '%s\\n' '{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"native-thread\",\"turnId\":\"native-turn\",\"itemId\":\"native-message\",\"delta\":\" from Codex\"}}'\n",
            "",
        );
    run_terminal_fixture(
        &without_deltas,
        "codex-scripted-completed-text",
        TurnStatus::Completed,
        None,
        ActivityStatus::Completed,
        Some(0),
    )
    .await;
}

#[tokio::test]
async fn codex_launch_protocol_and_process_failures_settle_as_error_activities() {
    let missing_directory = tempfile::tempdir().expect("create missing executable directory");
    assert_provider_failure(
        missing_directory.path().join("missing-codex"),
        "codex-missing-executable",
        "could not launch Codex app-server",
    )
    .await;

    let unlaunchable = tempfile::tempdir().expect("create unlaunchable executable directory");
    assert_provider_failure(
        unlaunchable.path(),
        "codex-spawn-failure",
        "could not launch Codex app-server",
    )
    .await;

    for (script, channel, expected) in [
        (
            INITIALIZE_REJECTION,
            "codex-initialize-rejection",
            "fixture rejected initialization",
        ),
        (MALFORMED_OUTPUT, "codex-malformed-output", "malformed JSON"),
        (
            EOF_WITH_PENDING_REQUEST,
            "codex-pending-request-eof",
            "Codex app-server",
        ),
        (
            TURN_REQUEST_ERROR,
            "codex-turn-request-error",
            "fixture rejected Turn startup",
        ),
        (
            EOF_AFTER_TURN_START,
            "codex-unexpected-eof",
            "Codex app-server",
        ),
        (NONZERO_AFTER_TURN_START, "codex-nonzero-exit", "status: 17"),
        (
            PROCESS_LOSS_WITH_UNSUPPORTED_REQUEST,
            "codex-process-loss-with-unsupported-request",
            "Codex app-server",
        ),
    ] {
        let fixture = ScriptedCodex::new(script);
        assert_provider_failure(fixture.executable(), channel, expected).await;
    }

    let oversized_message = format!("first\\nsecond {}", "diagnostic".repeat(300));
    let oversized_error =
        TURN_REQUEST_ERROR.replace("fixture rejected Turn startup", &oversized_message);
    let fixture = ScriptedCodex::new(&oversized_error);
    assert_provider_failure(
        fixture.executable(),
        "codex-oversized-request-error",
        "first second",
    )
    .await;
}

#[tokio::test]
async fn server_shutdown_interrupts_active_codex_and_allows_cooperative_exit() {
    let fixture = ScriptedCodex::new(COOPERATIVE_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-cooperative-shutdown")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-cooperative-shutdown")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep working until Chidori shuts down".to_owned(),
            },
        })
        .await
        .expect("create Session");
    fixture.wait_for_method("turn/start").await;
    fixture.wait_until_ready().await;
    let before_shutdown = wait_for_agent_output(&client, created.session.id).await;
    assert_eq!(before_shutdown.turns[0].status, TurnStatus::Active);

    let response = request_server_shutdown(&descriptor, ShutdownReason::Manual).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    fixture.wait_for_exit().await;

    let after_shutdown = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{}",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read Session during graceful HTTP shutdown")
        .error_for_status()
        .expect("Session remains readable during graceful HTTP shutdown")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session after Provider shutdown");
    assert_eq!(
        after_shutdown.revision, before_shutdown.revision,
        "Provider output after shutdown begins must not change the Session"
    );
    assert_eq!(after_shutdown.turns[0].status, TurnStatus::Active);

    let methods = fixture
        .requests()
        .into_iter()
        .filter_map(|request| {
            request
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "turn/interrupt"
        ]
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;

    drop(client);
    timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("repeated shutdown request remains bounded")
        .expect("shut down server");
}

#[tokio::test]
async fn server_shutdown_interrupts_a_turn_whose_start_response_is_pending() {
    let fixture = ScriptedCodex::new(PENDING_TURN_START_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-pending-turn-shutdown")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-pending-turn-shutdown")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Shut down while Codex accepts this Turn".to_owned(),
            },
        })
        .await
        .expect("create Session");
    fixture.wait_until_ready().await;

    let response = request_server_shutdown(&descriptor, ShutdownReason::Manual).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    fixture.release();
    timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("pending Turn startup keeps shutdown bounded")
        .expect("shut down server");

    assert!(
        fixture
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("turn/interrupt")),
        "shutdown waits for the accepted native Turn ID and interrupts it before closing transport"
    );
    assert_process_exited(fixture.pid()).await;
}

#[tokio::test]
async fn server_shutdown_releases_pending_rpc_and_forces_an_unresponsive_codex_to_exit() {
    let fixture = ScriptedCodex::new(UNRESPONSIVE_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-forced-shutdown").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-forced-shutdown")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Remain unresponsive during shutdown".to_owned(),
            },
        })
        .await
        .expect("create Session");
    fixture.wait_for_method("turn/start").await;
    fixture.wait_until_ready().await;
    wait_for_agent_output(&client, created.session.id).await;

    timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("forced Provider termination bounds server shutdown")
        .expect("shut down server");
    assert!(
        fixture
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("turn/interrupt")),
        "shutdown asks active Codex work to interrupt before forcing termination"
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;
}

#[tokio::test]
async fn server_shutdown_closes_transport_with_a_startup_request_pending() {
    let fixture = ScriptedCodex::new(PENDING_INITIALIZE_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-pending-startup-shutdown")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-pending-startup-shutdown")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Shut down during Codex initialization".to_owned(),
            },
        })
        .await
        .expect("create Session while Provider starts");
    fixture.wait_for_method("initialize").await;

    timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("pending startup RPC does not delay server shutdown")
        .expect("shut down server");
    assert!(
        fixture.exited.exists(),
        "server shutdown waits for cooperative Codex startup exit"
    );
    assert_process_exited(fixture.pid()).await;
}

#[tokio::test]
#[ignore = "set CHIDORI_CODEX_SMOKE=1 to use the installed authenticated Codex binary"]
async fn installed_codex_launches_runs_one_text_turn_and_shuts_down() {
    if std::env::var_os("CHIDORI_CODEX_SMOKE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("skipping: set CHIDORI_CODEX_SMOKE=1 to opt in");
        return;
    }

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-installed-smoke").expect("configure server"),
        Arc::new(CodexRuntime::from_environment()),
    )
    .await
    .expect("launch server with installed Codex");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-installed-smoke")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reply with a short confirmation that the smoke test completed.".to_owned(),
            },
        })
        .await
        .expect("create live Codex Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to live Codex Session SSE");

    let completed = timeout(Duration::from_secs(120), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read live Codex Session");
            if snapshot.turns.first().is_some_and(|turn| {
                matches!(
                    turn.status,
                    TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
                )
            }) {
                return snapshot;
            }
            feed.next()
                .await
                .expect("live Codex Session feed remains open")
                .expect("live Codex Session event is valid");
        }
    })
    .await
    .expect("installed Codex completes one text Turn");

    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    let selection = completed
        .session
        .agent_selection
        .expect("Codex publishes its effective Model");
    let identity = completed.turns[0]
        .agent
        .as_ref()
        .expect("Codex Turn captures its effective Agent");
    assert_eq!(identity.agent, AgentId::new("codex"));
    assert_eq!(identity.selection, selection);
    assert_eq!(selection.provider, ProviderId::new("codex"));
    assert!(completed.messages.iter().any(|message| {
        message.role == MessageRole::Agent
            && message.status == MessageStatus::Completed
            && !message.content.trim().is_empty()
    }));

    drop(feed);
    drop(client);
    timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("live Codex shutdown remains bounded")
        .expect("shut down live Codex server");
}

#[tokio::test]
async fn codex_retries_startup_with_a_fresh_thread_when_no_thread_id_exists() {
    let mut fixture = RecoveryFixture::start(
        STARTUP_RETRY,
        "codex-startup-retry",
        "Fail before a native Thread exists",
    )
    .await;
    fixture.wait_for_turn(0, TurnStatus::Failed).await;
    let recovered = fixture
        .admit_and_wait("Retry startup", 1, TurnStatus::Completed)
        .await;

    assert_eq!(recovered.session.id, fixture.session_id);
    assert_eq!(recovered.session.status, SessionStatus::Idle);
    assert_eq!(recovered.turns.len(), 2);
    assert_eq!(recovered.turns[0].status, TurnStatus::Failed);
    assert_eq!(recovered.turns[1].status, TurnStatus::Completed);
    let methods = fixture.codex.methods();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialize",
            "initialized",
            "thread/start",
            "turn/start"
        ]
    );
    assert!(!methods.iter().any(|method| method == "thread/resume"));

    fixture.shutdown().await;
}

#[tokio::test]
async fn codex_resumes_the_known_thread_after_active_process_loss() {
    let mut fixture = RecoveryFixture::start(
        ACTIVE_PROCESS_LOSS_THEN_RESUME,
        "codex-process-recovery",
        "Lose the first app-server",
    )
    .await;
    let failed = fixture.wait_for_turn(0, TurnStatus::Failed).await;
    assert_eq!(
        failed
            .activities
            .iter()
            .filter(|activity| matches!(activity, Activity::Error { .. }))
            .count(),
        1
    );
    assert!(failed.activities.iter().any(
        |activity| matches!(activity, Activity::Error { text, .. } if text.contains("exited unexpectedly"))
    ));

    let recovered = fixture
        .admit_and_wait(
            "Continue in the known Codex Thread",
            1,
            TurnStatus::Completed,
        )
        .await;
    assert_eq!(recovered.session.id, fixture.session_id);
    assert_eq!(
        recovered.session.agent_selection,
        failed.session.agent_selection
    );
    assert_eq!(recovered.turns.len(), 2);
    assert_eq!(recovered.messages.len(), 3);
    assert_eq!(
        recovered
            .messages
            .iter()
            .filter(|message| message.content == "Lose the first app-server")
            .count(),
        1,
        "resume must not project native history into the Chidori transcript"
    );
    assert_eq!(
        recovered
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("Recovered context")
    );
    assert_eq!(recovered.activities.len(), 1);

    let requests = fixture.codex.requests();
    assert_eq!(
        fixture.codex.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "initialize",
            "initialized",
            "thread/resume",
            "turn/start"
        ]
    );
    let resume = requests
        .iter()
        .find(|request| request["method"] == "thread/resume")
        .expect("second app-server resumes the known Thread");
    assert_eq!(resume["params"]["threadId"], "recoverable-thread");
    assert_eq!(
        resume["params"]["cwd"],
        fixture.workspace.path().to_string_lossy().as_ref()
    );
    assert_eq!(resume["params"]["approvalPolicy"], "never");
    assert_eq!(resume["params"]["sandbox"], "danger-full-access");
    assert_eq!(
        requests.last().expect("second Turn request")["params"]["threadId"],
        "recoverable-thread"
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn codex_surfaces_resume_rejection_without_starting_an_unrelated_thread() {
    let mut fixture = RecoveryFixture::start(
        ACTIVE_PROCESS_LOSS_THEN_RESUME_REJECTION,
        "codex-resume-rejection",
        "Lose the app-server",
    )
    .await;
    fixture.wait_for_turn(0, TurnStatus::Failed).await;
    let rejected = fixture
        .admit_and_wait("Attempt the required resume", 1, TurnStatus::Failed)
        .await;

    assert_eq!(rejected.session.id, fixture.session_id);
    assert_eq!(rejected.session.status, SessionStatus::Idle);
    assert_eq!(rejected.activities.len(), 2);
    assert!(matches!(
        &rejected.activities[1],
        Activity::Error { text, .. } if text.contains("fixture cannot resume this Thread")
    ));
    let methods = fixture.codex.methods();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "initialize",
            "initialized",
            "thread/resume"
        ]
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| **method == "thread/start")
            .count(),
        1,
        "resume rejection must not fall back to a new native Thread"
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn codex_resumes_on_the_first_prompt_after_process_loss_while_idle() {
    let mut fixture = RecoveryFixture::start(
        IDLE_PROCESS_LOSS_THEN_RESUME,
        "codex-idle-process-recovery",
        "Complete before the app-server exits",
    )
    .await;
    fixture.wait_for_turn(0, TurnStatus::Completed).await;
    fixture.codex.wait_for_exit().await;
    let resumed = fixture
        .admit_and_wait(
            "Resume immediately after idle loss",
            1,
            TurnStatus::Completed,
        )
        .await;

    assert_eq!(resumed.turns.len(), 2);
    assert_eq!(
        fixture.codex.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "initialize",
            "initialized",
            "thread/resume",
            "turn/start"
        ]
    );

    fixture.shutdown().await;
}

struct RecoveryFixture {
    codex: ScriptedCodex,
    _state_dir: tempfile::TempDir,
    workspace: tempfile::TempDir,
    server: RunningServer,
    client: ManagedClient,
    feed: SessionSubscription,
    session_id: SessionId,
}

impl RecoveryFixture {
    async fn start(script: &str, channel: &str, initial_prompt: &str) -> Self {
        let codex = ScriptedCodex::new_multiprocess(script);
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let server = server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            Arc::new(CodexRuntime::new(codex.executable())),
        )
        .await
        .expect("spawn server");
        let mut client = ManagedClient::connect(
            ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
        )
        .await
        .expect("connect client");
        receive_initial_state(&mut client).await;
        let created = client
            .create_session(CreateSessionRequest {
                agent_selection: None,
                workspace: Workspace {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: initial_prompt.to_owned(),
                },
            })
            .await
            .expect("create Session");
        let feed = client
            .subscribe_session(created.session.id)
            .await
            .expect("subscribe to Session SSE");
        Self {
            codex,
            _state_dir: state_dir,
            workspace,
            server,
            client,
            feed,
            session_id: created.session.id,
        }
    }

    async fn wait_for_turn(&mut self, turn_index: usize, expected: TurnStatus) -> SessionSnapshot {
        timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = self
                    .client
                    .read_session(self.session_id)
                    .await
                    .expect("read Session while waiting for Provider outcome");
                if snapshot
                    .turns
                    .get(turn_index)
                    .is_some_and(|turn| turn.status == expected)
                {
                    return snapshot;
                }
                self.feed
                    .next()
                    .await
                    .expect("Session feed remains open")
                    .expect("Session event is valid");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Turn {turn_index} reaches {expected:?}"))
    }

    async fn admit_and_wait(
        &mut self,
        prompt: &str,
        turn_index: usize,
        expected: TurnStatus,
    ) -> SessionSnapshot {
        self.client
            .admit_prompt(
                self.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: prompt.to_owned(),
                    },
                    delivery: PromptDelivery::Steer,
                },
            )
            .await
            .expect("admit recovery Prompt");
        self.wait_for_turn(turn_index, expected).await
    }

    async fn shutdown(self) {
        drop(self.feed);
        drop(self.client);
        self.server.shutdown().await.expect("shut down server");
    }
}

async fn assert_provider_failure(
    executable: impl AsRef<std::ffi::OsStr>,
    channel: &str,
    expected_error: &str,
) {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(executable)),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Surface the Provider failure".to_owned(),
            },
        })
        .await
        .expect("create Session before Provider startup settles");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");

    let failed = timeout(Duration::from_secs(2), async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session event is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session after Provider failure");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{channel} failure reaches a terminal Session state"));

    assert_eq!(failed.session.status, SessionStatus::Idle);
    assert_eq!(failed.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.activities.len(), 1);
    let Activity::Error { text, .. } = &failed.activities[0] else {
        panic!("Provider failure must be an Error Activity");
    };
    assert!(
        text.contains(expected_error),
        "expected {expected_error:?} in {text:?}"
    );
    assert!(!text.contains('\n'));
    assert!(
        text.chars().count() <= 512,
        "Provider failure Activity should remain concise: {text:?}"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

async fn assert_interruption_failure(script: &str, channel: &str, expected_error: &str) {
    let fixture = ScriptedCodex::new(&interruption_script(script));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Interrupt this work".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    feed.next()
        .await
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");
    fixture.wait_for_method("turn/start").await;
    let active = client
        .read_session(created.session.id)
        .await
        .expect("read active Session");

    let error = timeout(
        Duration::from_secs(7),
        client.interrupt_turn(created.session.id, active.turns[0].id),
    )
    .await
    .unwrap_or_else(|_| panic!("{channel} interruption command must not hang"))
    .expect_err("Provider interruption failure reaches the client");
    assert!(
        error.to_string().contains(expected_error),
        "expected {expected_error:?} in {error:#}"
    );

    let failed = timeout(Duration::from_secs(2), async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session after interruption failure");
            if snapshot.turns[0].status == TurnStatus::Failed {
                return snapshot;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{channel} failure reaches a terminal Session state"));
    assert_eq!(failed.session.status, SessionStatus::Idle);
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.activities.len(), 1);
    let Activity::Error { text, .. } = &failed.activities[0] else {
        panic!("interruption failure must project as an error Activity");
    };
    assert!(
        text.contains(expected_error),
        "expected {expected_error:?} in {text:?}"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

async fn run_terminal_fixture(
    script: &str,
    channel: &str,
    expected_status: TurnStatus,
    expected_error: Option<&str>,
    expected_command_status: ActivityStatus,
    expected_exit_status: Option<i32>,
) {
    let fixture = ScriptedCodex::new(script);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reach the requested terminal state".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    feed.next()
        .await
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");
    fixture.wait_for_method("turn/start").await;
    fixture.release();

    let completed = timeout(Duration::from_secs(2), async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == expected_status)
            {
                return snapshot;
            }
        }
    })
    .await
    .expect("native terminal outcome reaches Session SSE");

    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.turns[0].status, expected_status);
    assert!(
        completed
            .activities
            .iter()
            .any(|activity| matches!(activity,
        Activity::Command {
            status,
            exit_status,
            ..
        } if *status == expected_command_status && *exit_status == expected_exit_status))
    );
    assert_eq!(
        completed.messages.last().map(|message| message.status),
        Some(MessageStatus::Completed)
    );
    assert_eq!(
        completed
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("Hello from Codex")
    );
    match expected_error {
        Some(expected_error) => assert!(
            completed
                .activities
                .iter()
                .any(|activity| matches!(activity,
                    Activity::Error { text, .. } if text.contains(expected_error)))
        ),
        None => assert!(
            !completed
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Error { .. })),
            "a successful or interrupted Turn must not add an Error Activity"
        ),
    }

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

async fn receive_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next()).await,
        Ok(Some(ManagedEvent::Connecting))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next()).await,
        Ok(Some(ManagedEvent::Connected(_)))
    ));
}

async fn wait_for_agent_output(client: &ManagedClient, session_id: SessionId) -> SessionSnapshot {
    timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read Session while scripted Codex starts");
            if snapshot
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Agent)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scripted Codex Agent output reaches the Session")
}

async fn wait_for_session_snapshot(
    client: &ManagedClient,
    feed: &mut SessionSubscription,
    session_id: SessionId,
    description: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    timeout(Duration::from_secs(2), async {
        let mut observed_revision = 0;
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read observed Session");
            if predicate(&snapshot) && observed_revision >= snapshot.revision.0 {
                return snapshot;
            }
            observed_revision = match feed
                .next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid")
            {
                SessionEvent::Snapshot(snapshot) => snapshot.revision.0,
                SessionEvent::Updated(update) => update.revision.0,
            };
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description}"))
}

struct ScriptedCodex {
    _directory: tempfile::TempDir,
    executable: std::path::PathBuf,
    log: std::path::PathBuf,
    release: std::path::PathBuf,
    pid: std::path::PathBuf,
    exited: std::path::PathBuf,
    ready: std::path::PathBuf,
    child_pid: std::path::PathBuf,
}

impl ScriptedCodex {
    fn new_multiprocess(case_arms: &str) -> Self {
        Self::new(&format!(
            "{MULTIPROCESS_SCRIPT_PREFIX}{case_arms}  esac\ndone\n"
        ))
    }

    fn new(script: &str) -> Self {
        let directory = tempfile::tempdir().expect("create scripted Codex directory");
        let executable = directory.path().join("codex");
        let log = directory.path().join("requests.jsonl");
        let release = directory.path().join("release");
        let pid = directory.path().join("pid");
        let exited = directory.path().join("exited");
        let ready = directory.path().join("ready");
        let child_pid = directory.path().join("child-pid");
        let attempts = directory.path().join("attempts");
        let script = script
            .replace(
                "$CODEX_FIXTURE_LOG",
                log.to_str().expect("fixture log path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_RELEASE",
                release.to_str().expect("fixture release path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_PID",
                pid.to_str().expect("fixture PID path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_EXITED",
                exited.to_str().expect("fixture exit path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_READY",
                ready.to_str().expect("fixture ready path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_CHILD_PID",
                child_pid.to_str().expect("fixture child PID path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_ATTEMPTS",
                attempts.to_str().expect("fixture attempts path is UTF-8"),
            );
        std::fs::write(&executable, script).expect("write scripted Codex executable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("read scripted Codex metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).expect("make scripted Codex executable");
        Self {
            _directory: directory,
            executable,
            log,
            release,
            pid,
            exited,
            ready,
            child_pid,
        }
    }

    fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    async fn wait_for_method(&self, expected: &str) {
        self.wait_for_method_count(expected, 1).await;
    }

    async fn wait_for_method_count(&self, expected: &str, count: usize) {
        timeout(Duration::from_secs(2), async {
            loop {
                if self
                    .requests()
                    .iter()
                    .filter(|request| {
                        request.get("method").and_then(Value::as_str) == Some(expected)
                    })
                    .count()
                    >= count
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "scripted Codex receives {expected}; captured requests: {:?}",
                self.requests()
            )
        });
    }

    fn release_turn(&self, turn_index: usize) {
        std::fs::write(
            format!("{}-{turn_index}", self.release.display()),
            b"release",
        )
        .expect("release scripted Codex Turn");
    }

    fn release(&self) {
        std::fs::write(&self.release, b"release").expect("release scripted Codex events");
    }

    async fn wait_for_exit(&self) {
        timeout(Duration::from_secs(2), async {
            while !self.exited.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Codex exits cooperatively");
    }

    async fn wait_until_ready(&self) {
        timeout(Duration::from_secs(2), async {
            while !self.ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scripted Codex reports its native Turn ready");
    }

    fn pid(&self) -> u32 {
        std::fs::read_to_string(&self.pid)
            .expect("read scripted Codex PID")
            .trim()
            .parse()
            .expect("scripted Codex PID is numeric")
    }

    fn child_pid(&self) -> u32 {
        std::fs::read_to_string(&self.child_pid)
            .expect("read scripted Codex child PID")
            .trim()
            .parse()
            .expect("scripted Codex child PID is numeric")
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("decode captured Codex request"))
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter_map(|request| {
                request
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }
}

async fn assert_process_exited(pid: u32) {
    if timeout(Duration::from_secs(1), async {
        loop {
            if System::new_all().process(Pid::from_u32(pid)).is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_err()
    {
        let system = System::new_all();
        if let Some(process) = system.process(Pid::from_u32(pid)) {
            let _ = process.kill();
        }
        panic!("scripted Codex process {pid} survived server shutdown");
    }
}
