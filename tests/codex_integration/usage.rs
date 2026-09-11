//! Per-Turn Usage and Estimated Cost derived from Codex's cumulative metering.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{ScriptedCodex, receive_initial_state};
use axum::{Json, Router, routing::get};
use serde_json::json;
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    pricing::PricingSource,
    protocol::{
        Activity, AdmitPromptRequest, Cost, CostBasis, CreateSessionRequest, InitialPrompt,
        PromptDelivery, PromptId, SessionId, SessionSnapshot, TurnStatus, Usage,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use tokio::{
    net::TcpListener,
    time::{Duration, timeout},
};

/// A Codex whose thread meters two Turns as one running total, restating the
/// first Turn's figures unchanged before it settles. `__MODEL__` is the Model
/// the thread reports itself running, which is what the rate table is asked
/// about.
const METERED_TURNS_CODEX: &str = r#"#!/bin/sh
turn_index=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"__MODEL__"}}'
      ;;
    *'"method":"turn/start"'*)
      turn_index=$((turn_index + 1))
      response_id=$((turn_index + 2))
      if [ "$turn_index" -eq 1 ]; then
        printf '{"id":%s,"result":{"turn":{"id":"native-turn-1"}}}\n' "$response_id"
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"other-thread","turnId":"other-turn","tokenUsage":{"total":{"totalTokens":99999,"inputTokens":99999,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":99999,"reasoningOutputTokens":0},"last":{"totalTokens":99999,"inputTokens":99999,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":99999,"reasoningOutputTokens":0},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-1","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000,"futureField":true}}}'
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-1","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-1","status":"completed","items":[]}}}'
      else
        printf '{"id":%s,"result":{"turn":{"id":"native-turn-2"}}}\n' "$response_id"
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-2","tokenUsage":{"total":{"totalTokens":3800,"inputTokens":3100,"cachedInputTokens":900,"cacheWriteInputTokens":50,"outputTokens":700,"reasoningOutputTokens":150},"last":{"totalTokens":2450,"inputTokens":2000,"cachedInputTokens":800,"cacheWriteInputTokens":0,"outputTokens":450,"reasoningOutputTokens":100},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-2","status":"completed","items":[]}}}'
      fi
      ;;
  esac
done
"#;

/// A Codex that meters part of a Turn and then waits to be interrupted, so the
/// Turn settles on Usage it accrued before anything asked it to stop.
const INTERRUPTED_METERING_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"priced-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000}}}'
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":4,"result":{}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      ;;
  esac
done
"#;

/// A Codex whose thread carries a reading no Turn is waiting on — the shape a
/// reattach replays, and the shape a reading arriving after its Turn settled
/// takes — between the Turn that settled and the Turn that follows it.
const REPLAYED_READING_CODEX: &str = r#"#!/bin/sh
turn_index=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"priced-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      turn_index=$((turn_index + 1))
      response_id=$((turn_index + 2))
      if [ "$turn_index" -eq 1 ]; then
        printf '{"id":%s,"result":{"turn":{"id":"native-turn-1"}}}\n' "$response_id"
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-1","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-1","status":"completed","items":[]}}}'
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-1","tokenUsage":{"total":{"totalTokens":2400,"inputTokens":2000,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":400,"reasoningOutputTokens":50},"last":{"totalTokens":1050,"inputTokens":900,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":150,"reasoningOutputTokens":0},"modelContextWindow":272000}}}'
      else
        printf '{"id":%s,"result":{"turn":{"id":"native-turn-2"}}}\n' "$response_id"
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-2","tokenUsage":{"total":{"totalTokens":2600,"inputTokens":2150,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":450,"reasoningOutputTokens":50},"last":{"totalTokens":200,"inputTokens":150,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":50,"reasoningOutputTokens":0},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-2","status":"completed","items":[]}}}'
      fi
      ;;
  esac
done
"#;

/// A Codex delegating to a child thread that meters itself. The child's own
/// readings ride its own thread, so they must land in the child Session rather
/// than the parent's.
const DELEGATED_METERING_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"priced-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"child-priced-fixture"}}'
      sleep 0.05
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"child-thread","turnId":"child-turn","tokenUsage":{"total":{"totalTokens":600,"inputTokens":500,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":100,"reasoningOutputTokens":20},"last":{"totalTokens":600,"inputTokens":500,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":100,"reasoningOutputTokens":20},"modelContextWindow":272000}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-completed","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"root-thread","turnId":"root-turn","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
"#;

/// A child's first cumulative reading arrives before `thread/resume` supplies
/// Provider evidence of its Model. Only the distance travelled after that
/// reply can be estimated at the newly known rate.
const LATE_CHILD_MODEL_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"priced-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"child-thread","turnId":"child-turn","tokenUsage":{"total":{"totalTokens":120,"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":20,"reasoningOutputTokens":5},"last":{"totalTokens":120,"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":20,"reasoningOutputTokens":5},"modelContextWindow":272000}}}'
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"child-priced-fixture"}}'
      sleep 0.05
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"child-thread","turnId":"child-turn","tokenUsage":{"total":{"totalTokens":240,"inputTokens":200,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":40,"reasoningOutputTokens":10},"last":{"totalTokens":120,"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":20,"reasoningOutputTokens":5},"modelContextWindow":272000}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-completed","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
"#;

/// Once cumulative child Usage has crossed a Model boundary, switching back
/// to the original Model cannot make the later tokens attributable again.
const CHANGING_CHILD_MODEL_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"root-thread"},"model":"priced-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-spawn","kind":"started","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":4,"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"child-priced-fixture"}}'
      sleep 0.05
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"child-thread","turnId":"child-turn-a","tokenUsage":{"total":{"totalTokens":120,"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":20,"reasoningOutputTokens":5},"last":{"totalTokens":120,"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":20,"reasoningOutputTokens":5},"modelContextWindow":272000}}}'
      printf '%s\n' '{"method":"thread/settings/updated","params":{"threadId":"child-thread","threadSettings":{"model":"priced-fixture"}}}'
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"child-thread","turnId":"child-turn-b","tokenUsage":{"total":{"totalTokens":240,"inputTokens":200,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":40,"reasoningOutputTokens":10},"last":{"totalTokens":120,"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":20,"reasoningOutputTokens":5},"modelContextWindow":272000}}}'
      printf '%s\n' '{"method":"thread/settings/updated","params":{"threadId":"child-thread","threadSettings":{"model":"child-priced-fixture"}}}'
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"child-thread","turnId":"child-turn-a2","tokenUsage":{"total":{"totalTokens":360,"inputTokens":300,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":60,"reasoningOutputTokens":15},"last":{"totalTokens":120,"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":20,"reasoningOutputTokens":5},"modelContextWindow":272000}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn-a2","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"subAgentActivity","id":"activity-completed","kind":"completed","agentThreadId":"child-thread","agentPath":"/root/scout"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"root-thread","turn":{"id":"root-turn","status":"completed","items":[]}}}'
      ;;
"#;

/// A Codex that restates a settled Turn's figures late, while the Turn after
/// it is already running. The straggler is stale by construction: the running
/// Turn is still being measured, and must not be re-based onto it.
const STALE_READING_CODEX: &str = r#"#!/bin/sh
turn_index=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"priced-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      turn_index=$((turn_index + 1))
      response_id=$((turn_index + 2))
      if [ "$turn_index" -eq 1 ]; then
        printf '{"id":%s,"result":{"turn":{"id":"native-turn-1"}}}\n' "$response_id"
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-1","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-1","status":"completed","items":[]}}}'
      else
        printf '{"id":%s,"result":{"turn":{"id":"native-turn-2"}}}\n' "$response_id"
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-2","tokenUsage":{"total":{"totalTokens":2600,"inputTokens":2150,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":450,"reasoningOutputTokens":50},"last":{"totalTokens":1250,"inputTokens":1050,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":200,"reasoningOutputTokens":0},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-1","tokenUsage":{"total":{"totalTokens":1400,"inputTokens":1150,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":300,"reasoningOutputTokens":50},"last":{"totalTokens":50,"inputTokens":50,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":50,"reasoningOutputTokens":0},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"native-thread","turnId":"native-turn-2","tokenUsage":{"total":{"totalTokens":2900,"inputTokens":2400,"cachedInputTokens":150,"cacheWriteInputTokens":50,"outputTokens":500,"reasoningOutputTokens":80},"last":{"totalTokens":300,"inputTokens":250,"cachedInputTokens":50,"cacheWriteInputTokens":0,"outputTokens":50,"reasoningOutputTokens":30},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-2","status":"completed","items":[]}}}'
      fi
      ;;
  esac
done
"#;

/// A Codex thread that outlives a server restart. The reattach replays the
/// thread's persisted running total against the Turn it was measured under —
/// the one shape Codex replays — and the Turn that follows meters on from
/// there.
const PERSISTED_METERING_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"persisted-thread"},"model":"priced-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"persisted-thread"},"model":"priced-fixture"}}'
      printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"persisted-thread","turnId":"persisted-turn-1","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000}}}'
      touch "$CODEX_FIXTURE_READY"
      ;;
    *'"method":"turn/start"'*)
      if [ -e "$CODEX_FIXTURE_READY" ]; then
        printf '%s\n' '{"id":3,"result":{"turn":{"id":"persisted-turn-2"}}}'
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"persisted-thread","turnId":"persisted-turn-2","tokenUsage":{"total":{"totalTokens":2600,"inputTokens":2150,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":450,"reasoningOutputTokens":50},"last":{"totalTokens":1250,"inputTokens":1050,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":200,"reasoningOutputTokens":0},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"persisted-thread","turn":{"id":"persisted-turn-2","status":"completed","items":[]}}}'
      else
        printf '%s\n' '{"id":3,"result":{"turn":{"id":"persisted-turn-1"}}}'
        printf '%s\n' '{"method":"thread/tokenUsage/updated","params":{"threadId":"persisted-thread","turnId":"persisted-turn-1","tokenUsage":{"total":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"last":{"totalTokens":1350,"inputTokens":1100,"cachedInputTokens":100,"cacheWriteInputTokens":50,"outputTokens":250,"reasoningOutputTokens":50},"modelContextWindow":272000}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"persisted-thread","turn":{"id":"persisted-turn-1","status":"completed","items":[]}}}'
      fi
      ;;
"#;

/// What the first Turn consumed on its own: Codex's nested counts made
/// disjoint, and the context window it stated carried through.
fn first_turn_usage() -> Usage {
    Usage {
        fresh_input_tokens: Some(950),
        cache_read_tokens: Some(100),
        cache_write_tokens: Some(50),
        output_tokens: Some(200),
        reasoning_tokens: Some(50),
        native_meter: None,
        model_context_window: Some(272_000),
    }
}

/// A models.dev catalog serving one priced Model, and the Suru rate lookup
/// pointed at it. Tests asserting exact Costs begin with warm rates; cold and
/// refreshing rates have separate fixtures. Each cache has its own directory.
async fn priced_lookup() -> (Arc<PricingSource>, tempfile::TempDir) {
    let app = Router::new().route(
        "/api.json",
        get(|| async {
            Json(json!({
                "openai": {
                    "models": {
                        "priced-fixture": {
                            "id": "priced-fixture",
                            "cost": {
                                "input": 2.0,
                                "output": 4.0,
                                "cache_read": 0.5,
                                "cache_write": 0.25
                            }
                        },
                        "child-priced-fixture": {
                            "id": "child-priced-fixture",
                            "cost": {
                                "input": 10.0,
                                "output": 20.0,
                                "cache_read": 1.0,
                                "cache_write": 1.0
                            }
                        }
                    }
                }
            }))
        }),
    );
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind pricing fixture");
    let endpoint = format!("http://{}/api.json", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let cache_dir = tempfile::tempdir().expect("create pricing cache directory");
    let pricing = PricingSource::new(cache_dir.path())
        .with_source_endpoint(endpoint)
        .with_refresh_interval(Duration::from_secs(60))
        .with_fetch_timeout(Duration::from_secs(5));
    pricing.prime().await;
    (Arc::new(pricing), cache_dir)
}

/// A server hosting `codex` against the fixture rate table, a client past its
/// initial state, and a Session opened on `prompt`. `name` is the client
/// channel, so each test needs its own.
struct MeteredSession {
    server: server::RunningServer,
    client: ManagedClient,
    session_id: SessionId,
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    _pricing_cache: tempfile::TempDir,
}

async fn metered_session(
    codex: &ScriptedCodex,
    name: &'static str,
    prompt: &str,
) -> MeteredSession {
    let (pricing, pricing_cache) = priced_lookup().await;
    metered_session_with_pricing(codex, name, prompt, pricing, pricing_cache).await
}

async fn metered_session_with_pricing(
    codex: &ScriptedCodex,
    name: &'static str,
    prompt: &str,
    pricing: Arc<PricingSource>,
    pricing_cache: tempfile::TempDir,
) -> MeteredSession {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), name).expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable()).with_pricing_source(pricing)),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), name).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: prompt.to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    MeteredSession {
        server,
        client,
        session_id: created.session.id,
        _state_dir: state_dir,
        _workspace: workspace,
        _pricing_cache: pricing_cache,
    }
}

async fn settled_turn(client: &ManagedClient, session: SessionId, index: usize) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(session)
                .await
                .expect("read metered Session");
            if snapshot
                .turns
                .get(index)
                .is_some_and(|turn| turn.status != TurnStatus::Active)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("metered Turn settles")
}

#[tokio::test]
async fn codex_cumulative_readings_become_per_turn_deltas_priced_from_the_rate_table() {
    let fixture = ScriptedCodex::new(&METERED_TURNS_CODEX.replace("__MODEL__", "priced-fixture"));
    let opened = metered_session(&fixture, "codex-metered-turns", "Meter this Turn").await;
    let client = &opened.client;
    let first = settled_turn(client, opened.session_id, 0).await;

    assert_eq!(first.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        first.session.context_fill,
        Some(suru::protocol::ContextFill {
            occupied_tokens: 1350,
            capacity_tokens: Some(272_000)
        })
    );
    assert_eq!(
        first.turns[0].usage,
        Some(first_turn_usage()),
        "a re-fired reading restates the same total rather than adding to it"
    );
    assert_eq!(first.turns[0].cost, Cost::from_usd(0.0029625));
    assert_eq!(first.turns[0].cost_basis, Some(CostBasis::Estimated));

    client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Meter another Turn".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit second Prompt");
    let second = settled_turn(client, opened.session_id, 1).await;

    assert_eq!(
        second.turns[1].usage,
        Some(Usage {
            fresh_input_tokens: Some(1_200),
            cache_read_tokens: Some(800),
            cache_write_tokens: Some(0),
            output_tokens: Some(350),
            reasoning_tokens: Some(100),
            native_meter: None,
            model_context_window: Some(272_000),
        }),
        "the second Turn consumed the distance the thread's running total travelled"
    );
    assert_eq!(
        second.session.context_fill,
        Some(suru::protocol::ContextFill {
            occupied_tokens: 2450,
            capacity_tokens: Some(272_000)
        })
    );
    assert_eq!(second.turns[1].cost, Cost::from_usd(0.0046));
    assert_eq!(second.turns[1].cost_basis, Some(CostBasis::Estimated));
    assert_eq!(
        second.turns[0].usage,
        Some(first_turn_usage()),
        "a later reading does not reopen a settled Turn's record"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_reading_no_turn_is_waiting_on_sets_where_the_next_turn_measures_from() {
    let fixture = ScriptedCodex::new(REPLAYED_READING_CODEX);
    let opened = metered_session(
        &fixture,
        "codex-replayed-reading",
        "Meter across a Turn boundary",
    )
    .await;
    let client = &opened.client;
    let first = settled_turn(client, opened.session_id, 0).await;
    assert_eq!(first.turns[0].usage, Some(first_turn_usage()));

    client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Meter after the replay".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit second Prompt");
    let second = settled_turn(client, opened.session_id, 1).await;

    assert_eq!(
        second.turns[0].usage,
        Some(first_turn_usage()),
        "a reading arriving once a Turn has settled is not counted into it"
    );
    assert_eq!(
        second.turns[1].usage,
        Some(Usage {
            fresh_input_tokens: Some(150),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(50),
            reasoning_tokens: Some(0),
            native_meter: None,
            model_context_window: Some(272_000),
        }),
        "nor is it counted into the Turn that follows it"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_codex_model_the_rate_table_does_not_price_records_tokens_without_a_cost() {
    let fixture = ScriptedCodex::new(&METERED_TURNS_CODEX.replace("__MODEL__", "unpriced-fixture"));
    let opened = metered_session(&fixture, "codex-unpriced-model", "Meter an unpriced Model").await;
    let settled = settled_turn(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].usage, Some(first_turn_usage()));
    assert_eq!(
        settled.turns[0].cost, None,
        "an unknown price is absent rather than free"
    );
    assert_eq!(settled.turns[0].cost_basis, None);

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_child_threads_readings_land_in_the_subagents_own_session() {
    let fixture = ScriptedCodex::new_multiprocess(DELEGATED_METERING_CODEX);
    let opened = metered_session(&fixture, "codex-delegated-metering", "Delegate a survey").await;
    let parent = settled_turn(&opened.client, opened.session_id, 0).await;

    assert_eq!(
        parent.turns[0].usage,
        Some(first_turn_usage()),
        "the parent Turn records only what its own thread metered"
    );
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = parent
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
        .expect("the Transcript carries a Subagent row")
    else {
        unreachable!()
    };

    let child = settled_turn(&opened.client, *child_id, 0).await;
    assert_eq!(
        child.turns[0].usage,
        Some(Usage {
            fresh_input_tokens: Some(500),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(80),
            reasoning_tokens: Some(20),
            native_meter: None,
            model_context_window: Some(272_000),
        }),
        "the child's own readings fill the child Session's Turn"
    );
    assert_eq!(parent.session.context_fill.unwrap().occupied_tokens, 1350);
    assert_eq!(child.session.context_fill.unwrap().occupied_tokens, 600);
    assert_eq!(child.turns[0].cost, Cost::from_usd(0.007));
    assert_eq!(child.turns[0].cost_basis, Some(CostBasis::Estimated));

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn child_usage_before_model_evidence_is_never_priced_retroactively() {
    let fixture = ScriptedCodex::new_multiprocess(LATE_CHILD_MODEL_CODEX);
    let opened = metered_session(
        &fixture,
        "codex-late-child-model-metering",
        "Delegate before identity arrives",
    )
    .await;
    let parent = settled_turn(&opened.client, opened.session_id, 0).await;
    let Activity::Subagent {
        session_id: child_id,
        model,
        ..
    } = parent
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
        .expect("the Transcript carries a Subagent row")
    else {
        unreachable!()
    };
    assert_eq!(
        model.as_ref().map(|model| model.as_str()),
        Some("child-priced-fixture")
    );

    let child = settled_turn(&opened.client, *child_id, 0).await;
    assert_eq!(
        child.turns[0].usage,
        Some(Usage {
            fresh_input_tokens: Some(200),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(30),
            reasoning_tokens: Some(10),
            native_meter: None,
            model_context_window: Some(272_000),
        }),
        "all cumulative tokens remain visible after identity arrives"
    );
    assert_eq!(
        child.turns[0]
            .agent
            .as_ref()
            .map(|agent| agent.selection.model.as_str()),
        Some("child-priced-fixture")
    );
    assert_eq!(
        child.turns[0].cost,
        Cost::from_usd(0.0014),
        "only the 100 input and 20 output tokens after Model evidence are priced"
    );
    assert!(
        child.turns[0]
            .cost_details
            .as_ref()
            .is_some_and(|details| details.is_partial),
        "the known subtotal records the unpriced prefix"
    );

    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn switching_a_child_model_back_does_not_resume_ambiguous_estimates() {
    let fixture = ScriptedCodex::new_multiprocess(CHANGING_CHILD_MODEL_CODEX);
    let opened = metered_session(
        &fixture,
        "codex-changing-child-model-metering",
        "Delegate across Model changes",
    )
    .await;
    let parent = settled_turn(&opened.client, opened.session_id, 0).await;
    let Activity::Subagent {
        session_id: child_id,
        model,
        ..
    } = parent
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
        .expect("the Transcript carries a Subagent row")
    else {
        unreachable!()
    };
    assert_eq!(
        model.as_ref().map(|model| model.as_str()),
        Some("child-priced-fixture"),
        "the row presents the latest confirmed Model after A to B to A"
    );

    let child = settled_turn(&opened.client, *child_id, 0).await;
    assert_eq!(
        child.turns[0].usage,
        Some(Usage {
            fresh_input_tokens: Some(300),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(45),
            reasoning_tokens: Some(15),
            native_meter: None,
            model_context_window: Some(272_000),
        }),
        "tokens continue updating through both Model changes"
    );
    assert_eq!(
        child.turns[0]
            .agent
            .as_ref()
            .map(|agent| agent.selection.model.as_str()),
        Some("child-priced-fixture")
    );
    assert_eq!(
        child.turns[0].cost,
        Cost::from_usd(0.0014),
        "only the amount established before the first Model change is retained"
    );
    assert!(
        child.turns[0]
            .cost_details
            .as_ref()
            .is_some_and(|details| details.is_partial),
        "the cumulative Usage remains ambiguous after switching back"
    );

    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_interrupted_codex_turn_keeps_the_usage_it_had_accrued() {
    let fixture = ScriptedCodex::new(INTERRUPTED_METERING_CODEX);
    let opened = metered_session(
        &fixture,
        "codex-interrupted-metering",
        "Meter until interrupted",
    )
    .await;
    let client = &opened.client;
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(opened.session_id)
                .await
                .expect("read metering Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.usage.is_some())
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Usage streams before the Turn settles");

    client
        .interrupt_session(opened.session_id)
        .await
        .expect("Codex acknowledges interruption");
    let interrupted = settled_turn(client, opened.session_id, 0).await;

    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        interrupted.turns[0].usage,
        Some(first_turn_usage()),
        "an interrupted Turn keeps the partial Usage it accrued"
    );
    assert_eq!(interrupted.turns[0].cost, Cost::from_usd(0.0029625));
    assert_eq!(interrupted.turns[0].cost_basis, Some(CostBasis::Estimated));

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_settled_turns_late_reading_does_not_truncate_the_turn_now_running() {
    let fixture = ScriptedCodex::new(STALE_READING_CODEX);
    let opened = metered_session(&fixture, "codex-stale-reading", "Meter this Turn").await;
    let client = &opened.client;
    let first = settled_turn(client, opened.session_id, 0).await;
    assert_eq!(first.turns[0].usage, Some(first_turn_usage()));

    client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Meter through a straggler".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit second Prompt");
    let second = settled_turn(client, opened.session_id, 1).await;
    assert_eq!(
        second.session.context_fill.unwrap().occupied_tokens,
        300,
        "latest context can decrease without cumulative Usage decreasing"
    );

    assert_eq!(
        second.turns[1].usage,
        Some(Usage {
            fresh_input_tokens: Some(1_250),
            cache_read_tokens: Some(50),
            cache_write_tokens: Some(0),
            output_tokens: Some(220),
            reasoning_tokens: Some(30),
            native_meter: None,
            model_context_window: Some(272_000),
        }),
        "the running Turn keeps measuring from where it opened"
    );
    assert_eq!(
        second.turns[0].usage,
        Some(first_turn_usage()),
        "and the settled Turn's own record is not reopened by it"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn usage_survives_a_restart_and_the_reattach_replay_is_not_counted_again() {
    let fixture = ScriptedCodex::new_multiprocess(PERSISTED_METERING_CODEX);
    let (pricing, _pricing_cache) = priced_lookup().await;
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let channel = "codex-persisted-metering";
    let config = ServerConfig::new(state_dir.path(), channel)
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let client_config = || {
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure client")
            .with_data_dir(data_dir.path())
    };

    let original = server::spawn_with_provider(
        config.clone(),
        Arc::new(CodexRuntime::new(fixture.executable()).with_pricing_source(pricing.clone())),
    )
    .await
    .expect("spawn original server");
    let mut original_client = ManagedClient::connect(client_config())
        .await
        .expect("connect original client");
    receive_initial_state(&mut original_client).await;
    let created = original_client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Meter before the restart".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let before = settled_turn(&original_client, created.session.id, 0).await;
    assert_eq!(before.turns[0].usage, Some(first_turn_usage()));
    drop(original_client);
    original.shutdown().await.expect("stop original server");

    let replacement = server::spawn_with_provider(
        config,
        Arc::new(CodexRuntime::new(fixture.executable()).with_pricing_source(pricing)),
    )
    .await
    .expect("spawn replacement server");
    let mut replacement_client = ManagedClient::connect(client_config())
        .await
        .expect("connect replacement client");
    receive_initial_state(&mut replacement_client).await;
    let restored = replacement_client
        .read_session(created.session.id)
        .await
        .expect("read restored Session");
    assert_eq!(
        restored.turns[0].usage,
        Some(first_turn_usage()),
        "the Turn's record was persisted as it was recorded"
    );
    assert_eq!(restored.session.context_fill, before.session.context_fill);
    assert!(restored.session.context_fill.is_some());
    assert_eq!(restored.turns[0].cost, Cost::from_usd(0.0029625));
    assert_eq!(restored.turns[0].cost_basis, Some(CostBasis::Estimated));

    replacement_client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Meter after the restart".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit Prompt to the reopened Session");
    let after = settled_turn(&replacement_client, created.session.id, 1).await;

    assert_eq!(
        after.turns[1].usage,
        Some(Usage {
            fresh_input_tokens: Some(1_050),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(200),
            reasoning_tokens: Some(0),
            native_meter: None,
            model_context_window: Some(272_000),
        }),
        "the replayed total is where the new Turn measures from, not what it consumed"
    );
    assert_eq!(
        after.turns[0].usage,
        Some(first_turn_usage()),
        "and the restored Turn is not rewritten by the replay"
    );

    drop(replacement_client);
    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn a_stalled_pricing_fetch_does_not_delay_codex_usage_or_settlement() {
    stalled_pricing_does_not_delay_turn(false).await;
}

#[tokio::test]
async fn a_stalled_overdue_refresh_does_not_delay_codex_usage_or_settlement() {
    stalled_pricing_does_not_delay_turn(true).await;
}

async fn stalled_pricing_does_not_delay_turn(initially_warm: bool) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let handler_entered = entered.clone();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = Router::new().route(
        "/api.json",
        get(move || {
            let entered = handler_entered.clone();
            let requests = requests.clone();
            async move {
                if initially_warm && requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
                {
                    return Json(json!({"openai":{"models":{"priced-fixture":{"cost":{
                        "input":2,"output":4,"cache_read":0.5,"cache_write":0.25
                    }}}}}));
                }
                entered.notify_one();
                std::future::pending::<Json<serde_json::Value>>().await
            }
        }),
    );
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}/api.json", listener.local_addr().unwrap());
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cache = tempfile::tempdir().unwrap();
    let pricing = Arc::new(
        PricingSource::new(cache.path())
            .with_source_endpoint(endpoint)
            .with_refresh_interval(Duration::from_millis(20))
            .with_fetch_timeout(Duration::from_millis(1500)),
    );
    if initially_warm {
        pricing.prime().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let warming = pricing.clone();
    let prime = tokio::spawn(async move { warming.prime().await });
    timeout(PROGRESS_DEADLINE, entered.notified())
        .await
        .unwrap();
    let fixture = ScriptedCodex::new(&METERED_TURNS_CODEX.replace("__MODEL__", "priced-fixture"));
    let opened = metered_session_with_pricing(
        &fixture,
        "codex-stalled-pricing",
        "Meter while pricing hangs",
        pricing,
        cache,
    )
    .await;
    let settled = timeout(
        Duration::from_millis(500),
        settled_turn(&opened.client, opened.session_id, 0),
    )
    .await
    .expect("pricing must not hold up the Turn");
    assert_eq!(settled.turns[0].usage, Some(first_turn_usage()));
    assert_eq!(settled.turns[0].cost, None);
    assert_eq!(settled.turns[0].cost_basis, None);
    opened.server.shutdown().await.unwrap();
    prime.abort();
    http.abort();
}

#[tokio::test]
async fn a_cold_table_warming_mid_turn_prices_a_later_reading() {
    pricing_recovers_during_turn(false).await;
}

#[tokio::test]
async fn a_long_lived_session_refreshes_rates_without_waiting_for_an_event() {
    pricing_recovers_during_turn(true).await;
}

async fn pricing_recovers_during_turn(refresh: bool) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_entered = entered.clone();
    let handler_release = release.clone();
    let app = Router::new().route(
        "/api.json",
        get(move || {
            let entered = handler_entered.clone();
            let release = handler_release.clone();
            async move {
                entered.notify_one();
                release.acquire().await.unwrap().forget();
                Json(json!({"openai":{"models":{"priced-fixture":{"cost":{
                    "input":2,"output":4,"cache_read":0.5,"cache_write":0.25
                }}}}}))
            }
        }),
    );
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}/api.json", listener.local_addr().unwrap());
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cache = tempfile::tempdir().unwrap();
    let pricing = Arc::new(
        PricingSource::new(cache.path())
            .with_source_endpoint(endpoint)
            .with_refresh_interval(Duration::from_millis(800))
            .with_fetch_timeout(Duration::from_millis(1500)),
    );
    if refresh {
        release.add_permits(1);
        pricing.prime().await;
        entered.notified().await;
    }
    // Restate Usage at interruption, giving the test control over the next
    // reading without sleeping in the scripted Provider.
    let reading = INTERRUPTED_METERING_CODEX
        .lines()
        .find(|line| line.contains("thread/tokenUsage/updated"))
        .unwrap();
    let script = INTERRUPTED_METERING_CODEX.replace(
        "    *'\"method\":\"turn/interrupt\"'*)",
        &format!("    *'\"method\":\"turn/interrupt\"'*)\n{reading}"),
    );
    let fixture = ScriptedCodex::new(&script);
    let opened = metered_session_with_pricing(
        &fixture,
        "codex-pricing-recovers",
        "Meter as rates warm",
        pricing.clone(),
        cache,
    )
    .await;
    let first = timeout(Duration::from_millis(500), async {
        loop {
            let snapshot = opened.client.read_session(opened.session_id).await.unwrap();
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.usage.is_some())
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Usage flows before the pricing response");
    assert_eq!(first.turns[0].usage, Some(first_turn_usage()));
    if !refresh {
        assert_eq!(first.turns[0].cost, None);
    }
    timeout(Duration::from_millis(1200), entered.notified())
        .await
        .expect("Session starts the fetch without another Provider event");
    release.add_permits(1);
    timeout(Duration::from_millis(500), async {
        loop {
            if pricing
                .estimate_cached(
                    &suru::pricing::ModelsDevModel::new(
                        "openai",
                        suru::protocol::ModelId::new("priced-fixture"),
                    ),
                    &first_turn_usage(),
                )
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("rates become readable");
    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .unwrap();
    let settled = settled_turn(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].cost, Cost::from_usd(0.0029625));
    assert_eq!(settled.turns[0].cost_basis, Some(CostBasis::Estimated));
    if refresh {
        timeout(Duration::from_millis(1200), entered.notified())
            .await
            .expect("interrupting a Turn leaves the Session's refresh cadence alive");
    }
    opened.server.shutdown().await.unwrap();
    http.abort();
}

#[tokio::test]
async fn codex_context_fill_distinguishes_zero_missing_and_nonpositive_capacity() {
    for (last, capacity, expected) in [
        ("0", "272000", Some((0, Some(272_000)))),
        ("1350", "0", Some((1350, None))),
        ("1350", "-1", Some((1350, None))),
        ("null", "272000", None),
    ] {
        let script = METERED_TURNS_CODEX
            .replace("__MODEL__", "priced-fixture")
            .replace(
                "\"last\":{\"totalTokens\":1350",
                &format!("\"last\":{{\"totalTokens\":{last}"),
            )
            .replace(
                "\"modelContextWindow\":272000",
                &format!("\"modelContextWindow\":{capacity}"),
            );
        let fixture = ScriptedCodex::new(&script);
        let opened = metered_session(&fixture, "codex-partial-context", "Read context").await;
        let settled = settled_turn(&opened.client, opened.session_id, 0).await;
        assert_eq!(
            settled
                .session
                .context_fill
                .map(|fill| (fill.occupied_tokens, fill.capacity_tokens)),
            expected
        );
        assert_eq!(
            settled.turns[0].usage.as_ref().unwrap().fresh_input_tokens,
            Some(950),
            "Usage remains independent of occupancy"
        );
        opened.server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn superseded_child_native_turns_cannot_replace_current_or_settled_context() {
    fn report(turn: &str, occupied: u64, cumulative_input: u64) -> String {
        let event = json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": "child-thread", "turnId": turn,
                "tokenUsage": {
                    "total": { "totalTokens": cumulative_input + 100, "inputTokens": cumulative_input,
                        "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 100, "reasoningOutputTokens": 20 },
                    "last": { "totalTokens": occupied }, "modelContextWindow": 272000
                }
            }
        });
        format!("      printf '%s\\n' '{event}'")
    }
    for after_settlement in [false, true] {
        let original_report = DELEGATED_METERING_CODEX
            .lines()
            .find(|line| {
                line.contains("thread/tokenUsage/updated") && line.contains("child-thread")
            })
            .expect("fixture meters its child");
        let newer_turn = r#"      printf '%s\n' '{"method":"turn/started","params":{"threadId":"child-thread","turn":{"id":"child-turn-2","status":"inProgress","items":[]}}}'"#;
        let script = DELEGATED_METERING_CODEX.replace(
            original_report,
            &format!(
                "{original_report}\n{newer_turn}\n{}\n{}",
                report("child-turn-2", 300, 900),
                report("child-turn", 9999, 1000),
            ),
        );
        let script = if after_settlement {
            let settle = script
                .lines()
                .find(|line| line.contains("activity-completed"))
                .unwrap();
            script.replace(
                settle,
                &format!(
                    "{settle}\n{}\n{}",
                    report("child-turn-2", 100, 1100),
                    report("child-turn", 99_999, 1200)
                ),
            )
        } else {
            script
        };
        let fixture = ScriptedCodex::new_multiprocess(&script);
        let opened =
            metered_session(&fixture, "codex-child-context-order", "Observe child Turns").await;
        // The root settles after all child reports, providing a wire-order barrier.
        let parent = settled_turn(&opened.client, opened.session_id, 0).await;
        let child_id = parent
            .activities
            .iter()
            .find_map(|activity| match activity {
                Activity::Subagent { session_id, .. } => Some(*session_id),
                _ => None,
            })
            .unwrap();
        let child = settled_turn(&opened.client, child_id, 0).await;
        assert_eq!(
            child.session.context_fill.unwrap().occupied_tokens,
            if after_settlement { 100 } else { 300 }
        );
        assert_eq!(parent.session.context_fill.unwrap().occupied_tokens, 1350);
        assert_eq!(
            child.turns[0].usage.as_ref().unwrap().fresh_input_tokens,
            Some(1000),
            "Context ordering leaves existing cumulative child Usage admission unchanged"
        );
        assert_eq!(parent.turns[0].usage, Some(first_turn_usage()));
        opened.server.shutdown().await.unwrap();
    }
}
