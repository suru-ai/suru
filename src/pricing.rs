use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::protocol::{Cost, CostBasis, ModelId, Usage};
use crate::runtime::protect_current_user_file;

const DEFAULT_SOURCE_ENDPOINT: &str = "https://models.dev/api.json";
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const CACHE_FILE: &str = "models-dev-pricing.json";
const USD_PER_MILLION_TOKENS: f64 = 1_000_000.0;

/// One Model as models.dev identifies it. The Provider identifier belongs to
/// the pricing catalog rather than Suru's Provider vocabulary: for example,
/// Codex Models use models.dev's `openai` catalog.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModelsDevModel {
    provider: String,
    model: ModelId,
}

impl ModelsDevModel {
    pub fn new(provider: impl Into<String>, model: ModelId) -> Self {
        Self {
            provider: provider.into(),
            model,
        }
    }
}

/// A Cost computed from the rate table. Its Basis is fixed by construction so
/// a caller cannot accidentally record an estimate as Provider-reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EstimatedCost {
    cost: Cost,
}

impl EstimatedCost {
    pub const fn cost(self) -> Cost {
        self.cost
    }

    pub const fn basis(self) -> CostBasis {
        CostBasis::Estimated
    }
}

/// Best-effort models.dev pricing behind one lookup interface. The source's
/// Provider nesting, USD-per-million units, durable cache, refresh cadence,
/// and failures remain internal; callers only supply a Model and its Usage.
#[derive(Debug)]
pub struct PricingSource {
    cache_path: PathBuf,
    source_endpoint: String,
    refresh_interval: Duration,
    fetch_timeout: Duration,
    http: reqwest::Client,
    state: Mutex<PricingState>,
}

pub(crate) struct PricingRefresh(tokio::task::JoinHandle<()>);

impl PricingRefresh {
    pub(crate) fn stop(&self) {
        self.0.abort();
    }
}

impl Drop for PricingRefresh {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug, Default)]
struct PricingState {
    cache_loaded: bool,
    document: Option<CacheDocument>,
    last_attempted_at_millis: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheDocument {
    last_attempted_at_millis: u64,
    catalog: RateCatalog,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RateCatalog {
    providers: HashMap<String, HashMap<String, ModelRates>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct ModelRates {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl PricingSource {
    /// Uses the canonical models.dev endpoint, a daily refresh interval, and
    /// stores the normalized rate table under `data_dir` when possible.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            cache_path: data_dir.as_ref().join(CACHE_FILE),
            source_endpoint: DEFAULT_SOURCE_ENDPOINT.to_owned(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            fetch_timeout: DEFAULT_FETCH_TIMEOUT,
            http: reqwest::Client::new(),
            state: Mutex::new(PricingState::default()),
        }
    }

    /// Replaces the models.dev endpoint. Intended for fixture-backed tests and
    /// deployments that mirror the catalog.
    pub fn with_source_endpoint(mut self, source_endpoint: impl Into<String>) -> Self {
        self.source_endpoint = source_endpoint.into();
        self
    }

    /// Replaces the minimum interval between fetch attempts. Failed attempts
    /// are throttled too, so an outage cannot turn lookups into network churn.
    pub fn with_refresh_interval(mut self, refresh_interval: Duration) -> Self {
        self.refresh_interval = refresh_interval;
        self
    }

    /// Bounds one complete source fetch, including its response body. Intended
    /// for millisecond-scale fixture timeouts as well as production tuning.
    pub fn with_fetch_timeout(mut self, fetch_timeout: Duration) -> Self {
        self.fetch_timeout = fetch_timeout;
        self
    }

    /// Fetches the rate table if it is due, so a later lookup reads a warm
    /// cache instead of waiting on the network. Callers run this away from any
    /// path a user is waiting on: the fetch is bounded, but a Turn's output
    /// should never be held behind it.
    pub async fn prime(&self) {
        let _ = self.fetch_catalog().await;
    }

    /// Reads only fresh in-memory rates. Never waits for a fetch or its lock,
    /// loads a disk cache, or starts I/O. Cold, busy, and overdue tables yield
    /// absence so streaming Provider output can always keep moving.
    pub fn estimate_cached(&self, model: &ModelsDevModel, usage: &Usage) -> Option<EstimatedCost> {
        let state = self.state.try_lock().ok()?;
        let document = state.document.as_ref()?;
        if !within_refresh_interval(
            document.last_attempted_at_millis,
            now_millis(),
            self.refresh_interval,
        ) {
            return None;
        }
        document.catalog.estimate(model, usage)
    }

    /// Keeps rates current while a consumer is alive. Multiple consumers share
    /// the fetch throttle; dropping the guard cancels this consumer's task.
    pub(crate) fn keep_fresh(self: &std::sync::Arc<Self>) -> PricingRefresh {
        let source = self.clone();
        PricingRefresh(tokio::spawn(async move {
            loop {
                source.prime().await;
                let delay = {
                    let state = source.state.lock().await;
                    let elapsed = now_millis()
                        .saturating_sub(state.last_attempted_at_millis.unwrap_or_default());
                    source
                        .refresh_interval
                        .saturating_sub(Duration::from_millis(elapsed))
                };
                tokio::time::sleep(delay.max(Duration::from_millis(1))).await;
            }
        }))
    }

    /// Prices every reported token part at the Model's own catalog rates;
    /// Reasoning uses the output rate. Unknown Models, missing rates needed by
    /// the Usage, malformed entries, and unavailable first-run catalogs all
    /// yield absence without surfacing a lookup error. May fetch and wait;
    /// streaming callers must use `estimate_cached` instead.
    pub async fn estimate(&self, model: &ModelsDevModel, usage: &Usage) -> Option<EstimatedCost> {
        let catalog = self.fetch_catalog().await?;
        catalog.estimate(model, usage)
    }

    async fn fetch_catalog(&self) -> Option<RateCatalog> {
        let mut state = self.state.lock().await;
        if !state.cache_loaded {
            state.document = read_cache(&self.cache_path);
            state.last_attempted_at_millis = state
                .document
                .as_ref()
                .map(|document| document.last_attempted_at_millis);
            state.cache_loaded = true;
        }

        let attempted_at_millis = now_millis();
        if state.last_attempted_at_millis.is_some_and(|last_attempt| {
            within_refresh_interval(last_attempt, attempted_at_millis, self.refresh_interval)
        }) {
            return state
                .document
                .as_ref()
                .map(|document| document.catalog.clone());
        }

        let fetched = self.fetch_source().await;
        // The lock already serializes fetches. Record only completed attempts
        // so cancelling a consumer cannot throttle its replacement for a day.
        state.last_attempted_at_millis = Some(attempted_at_millis);
        let source = match fetched {
            Some(source) => source,
            None => return self.retain_cache_after_failure(&mut state, attempted_at_millis),
        };
        let catalog = match RateCatalog::from_models_dev(&source) {
            Some(catalog) => catalog,
            None => {
                tracing::warn!("models.dev pricing response has no Model catalog");
                return self.retain_cache_after_failure(&mut state, attempted_at_millis);
            }
        };
        let document = CacheDocument {
            last_attempted_at_millis: attempted_at_millis,
            catalog: catalog.clone(),
        };
        if write_cache(&self.cache_path, &document).is_err() {
            tracing::warn!(path = ?self.cache_path, "failed to cache models.dev pricing");
        }
        state.document = Some(document);
        Some(catalog)
    }

    fn retain_cache_after_failure(
        &self,
        state: &mut PricingState,
        attempted_at_millis: u64,
    ) -> Option<RateCatalog> {
        if let Some(document) = state.document.as_mut() {
            document.last_attempted_at_millis = attempted_at_millis;
            if write_cache(&self.cache_path, document).is_err() {
                tracing::warn!(path = ?self.cache_path, "failed to retain models.dev pricing cache");
            }
        }
        state
            .document
            .as_ref()
            .map(|document| document.catalog.clone())
    }

    async fn fetch_source(&self) -> Option<Value> {
        let fetch = async {
            self.http
                .get(&self.source_endpoint)
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await
        };
        match tokio::time::timeout(self.fetch_timeout, fetch).await {
            Ok(Ok(source)) => Some(source),
            Ok(Err(error)) => {
                tracing::warn!(%error, "failed to fetch models.dev pricing");
                None
            }
            Err(_) => {
                tracing::warn!(timeout = ?self.fetch_timeout, "models.dev pricing fetch timed out");
                None
            }
        }
    }
}

impl RateCatalog {
    fn estimate(&self, model: &ModelsDevModel, usage: &Usage) -> Option<EstimatedCost> {
        let cost = self
            .providers
            .get(&model.provider)?
            .get(model.model.as_str())?
            .price(usage)?;
        Some(EstimatedCost { cost })
    }

    fn from_models_dev(source: &Value) -> Option<Self> {
        let source = source.as_object()?;
        let mut providers = HashMap::new();
        let mut found_catalog = false;
        for (provider_id, provider) in source {
            let Some(models) = provider.get("models").and_then(Value::as_object) else {
                continue;
            };
            found_catalog = true;
            let mut normalized = HashMap::new();
            for (key, model) in models {
                let Some(rates) = model.get("cost").and_then(ModelRates::from_models_dev) else {
                    continue;
                };
                normalized.insert(key.clone(), rates);
                if let Some(id) = model.get("id").and_then(Value::as_str) {
                    normalized.insert(id.to_owned(), rates);
                }
            }
            providers.insert(provider_id.clone(), normalized);
        }
        found_catalog.then_some(Self { providers })
    }
}

impl ModelRates {
    fn from_models_dev(cost: &Value) -> Option<Self> {
        let input = valid_rate(cost.get("input")?)?;
        let output = valid_rate(cost.get("output")?)?;
        let cache_read = optional_rate(cost.get("cache_read"))?;
        let cache_write = optional_rate(cost.get("cache_write"))?;
        Some(Self {
            input,
            output,
            cache_read,
            cache_write,
        })
    }

    fn price(self, usage: &Usage) -> Option<Cost> {
        let usd = [
            priced_tokens(usage.fresh_input_tokens, Some(self.input))?,
            priced_tokens(usage.cache_read_tokens, self.cache_read)?,
            priced_tokens(usage.cache_write_tokens, self.cache_write)?,
            priced_tokens(usage.output_tokens, Some(self.output))?,
            priced_tokens(usage.reasoning_tokens, Some(self.output))?,
        ]
        .into_iter()
        .sum();
        Cost::from_usd(usd)
    }

    fn is_valid(self) -> bool {
        [
            Some(self.input),
            Some(self.output),
            self.cache_read,
            self.cache_write,
        ]
        .into_iter()
        .flatten()
        .all(rate_is_valid)
    }
}

fn valid_rate(value: &Value) -> Option<f64> {
    value.as_f64().filter(|rate| rate_is_valid(*rate))
}

fn optional_rate(value: Option<&Value>) -> Option<Option<f64>> {
    match value {
        Some(value) => valid_rate(value).map(Some),
        None => Some(None),
    }
}

fn priced_tokens(tokens: Option<u64>, rate: Option<f64>) -> Option<f64> {
    let Some(tokens) = tokens else {
        return Some(0.0);
    };
    if tokens == 0 {
        return Some(0.0);
    }
    let usd = tokens as f64 * rate? / USD_PER_MILLION_TOKENS;
    usd.is_finite().then_some(usd)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn within_refresh_interval(
    attempted_at_millis: u64,
    now_millis: u64,
    refresh_interval: Duration,
) -> bool {
    let refresh_millis = u64::try_from(refresh_interval.as_millis()).unwrap_or(u64::MAX);
    now_millis.saturating_sub(attempted_at_millis) < refresh_millis
}

fn read_cache(path: &Path) -> Option<CacheDocument> {
    let bytes = fs::read(path).ok()?;
    let mut document: CacheDocument = serde_json::from_slice(&bytes).ok()?;
    for models in document.catalog.providers.values_mut() {
        models.retain(|_, rates| rates.is_valid());
    }
    Some(document)
}

fn rate_is_valid(rate: f64) -> bool {
    rate.is_finite() && rate >= 0.0
}

fn write_cache(path: &Path, document: &CacheDocument) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(document)?;
    fs::write(path, bytes)?;
    protect_current_user_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    fn openai_model(id: &str) -> ModelsDevModel {
        ModelsDevModel::new("openai", ModelId::new(id))
    }

    /// Long enough that no lookup in a test falls due on its own, however slow
    /// the machine; tests make a refresh due with `let_the_interval_pass`.
    const TEST_REFRESH_INTERVAL: Duration = Duration::from_secs(3600);

    /// Moves the last fetch attempt back past any refresh interval, standing
    /// in for the wait so no assertion races the wall clock.
    async fn let_the_interval_pass(pricing: &PricingSource) {
        let mut state = pricing.state.lock().await;
        state.last_attempted_at_millis = Some(0);
        if let Some(document) = state.document.as_mut() {
            document.last_attempted_at_millis = 0;
        }
    }

    async fn serve_fixture(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind pricing fixture");
        let endpoint = format!("http://{}/api.json", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve pricing fixture");
        });
        (endpoint, server)
    }

    #[tokio::test]
    async fn cancelling_a_fetch_leaves_cached_reads_free_and_a_new_fetch_possible() {
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let handler_entered = entered.clone();
        let requests = std::sync::Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let entered = handler_entered.clone();
                let requests = requests.clone();
                async move {
                    if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Json(json!({"openai":{"models":{"fixture":{"cost":{"input":2,"output":4}}}}}))
                }
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().unwrap();
        let pricing = std::sync::Arc::new(
            PricingSource::new(data_dir.path())
                .with_source_endpoint(endpoint)
                .with_fetch_timeout(Duration::from_millis(500)),
        );
        let source = pricing.clone();
        let prime = tokio::spawn(async move { source.prime().await });
        tokio::time::timeout(Duration::from_millis(200), entered.notified())
            .await
            .unwrap();
        let model = openai_model("fixture");
        let usage = Usage {
            fresh_input_tokens: Some(500_000),
            ..Usage::default()
        };
        assert_eq!(pricing.estimate_cached(&model, &usage), None);
        prime.abort();
        assert!(prime.await.unwrap_err().is_cancelled());
        pricing.prime().await;
        assert_eq!(
            pricing
                .estimate_cached(&model, &usage)
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        server.abort();
    }

    #[tokio::test]
    async fn fixture_rates_price_each_usage_part_and_are_cached_durably() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let request_count = request_count.clone();
                async move {
                    request_count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "openai": {
                            "models": {
                                "priced-model": {
                                    "id": "priced-model",
                                    "cost": {
                                        "input": 2.0,
                                        "output": 4.0,
                                        "cache_read": 0.5,
                                        "cache_write": 0.25
                                    }
                                }
                            }
                        },
                        "alternative": {
                            "models": {
                                "priced-model": {
                                    "cost": { "input": 20.0, "output": 40.0 }
                                }
                            }
                        }
                    }))
                }
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().expect("create data directory");
        let pricing = PricingSource::new(data_dir.path()).with_source_endpoint(endpoint);
        let usage = Usage {
            fresh_input_tokens: Some(1_000_000),
            cache_read_tokens: Some(2_000_000),
            cache_write_tokens: Some(3_000_000),
            output_tokens: Some(4_000_000),
            reasoning_tokens: Some(5_000_000),
            ..Usage::default()
        };

        let estimated = pricing
            .estimate(&openai_model("priced-model"), &usage)
            .await
            .expect("fixture Model has valid rates");

        assert_eq!(estimated.cost(), Cost::from_usd(39.75).unwrap());
        assert_eq!(estimated.basis(), CostBasis::Estimated);
        assert_eq!(
            pricing
                .estimate(
                    &ModelsDevModel::new("alternative", ModelId::new("priced-model")),
                    &Usage {
                        fresh_input_tokens: Some(1_000_000),
                        ..Usage::default()
                    },
                )
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(20.0)
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(data_dir.path().join("models-dev-pricing.json").is_file());
        server.abort();
    }

    #[tokio::test]
    async fn unknown_and_malformed_rates_degrade_to_absent_cost() {
        let app = Router::new().route(
            "/api.json",
            get(|| async {
                Json(json!({
                    "openai": {
                        "models": {
                            "negative-rate": {
                                "cost": { "input": -1.0, "output": 4.0 }
                            },
                            "non-numeric-rate": {
                                "cost": { "input": "NaN", "output": 4.0 }
                            },
                            "unrepresentable-cost": {
                                "cost": { "input": 1e308, "output": 4.0 }
                            }
                        }
                    }
                }))
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().expect("create data directory");
        let pricing = PricingSource::new(data_dir.path()).with_source_endpoint(endpoint);
        let ordinary_usage = Usage {
            fresh_input_tokens: Some(100),
            ..Usage::default()
        };

        assert_eq!(
            pricing
                .estimate(&openai_model("unknown-model"), &ordinary_usage)
                .await,
            None
        );
        assert_eq!(
            pricing
                .estimate(&openai_model("negative-rate"), &ordinary_usage)
                .await,
            None
        );
        assert_eq!(
            pricing
                .estimate(&openai_model("non-numeric-rate"), &ordinary_usage)
                .await,
            None
        );
        assert_eq!(
            pricing
                .estimate(
                    &openai_model("unrepresentable-cost"),
                    &Usage {
                        fresh_input_tokens: Some(u64::MAX),
                        ..Usage::default()
                    },
                )
                .await,
            None
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_fresh_durable_cache_is_reused_after_restart_without_fetching() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let request_count = request_count.clone();
                async move {
                    request_count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "openai": {
                            "models": {
                                "cached-model": {
                                    "cost": { "input": 2.0, "output": 4.0 }
                                }
                            }
                        }
                    }))
                }
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().expect("create data directory");
        let usage = Usage {
            fresh_input_tokens: Some(500_000),
            ..Usage::default()
        };
        let first = PricingSource::new(data_dir.path()).with_source_endpoint(endpoint.clone());
        assert_eq!(
            first
                .estimate(&openai_model("cached-model"), &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        server.abort();
        let _ = server.await;

        let restarted = PricingSource::new(data_dir.path()).with_source_endpoint(endpoint);
        assert_eq!(
            restarted
                .estimate(&openai_model("cached-model"), &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_honors_the_injected_interval_and_reuses_the_cache_between_lookups() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let input_rate = Arc::new(AtomicU64::new(2));
        let served_rate = input_rate.clone();
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let request_count = request_count.clone();
                let served_rate = served_rate.clone();
                async move {
                    request_count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "openai": {
                            "models": {
                                "changing-model": {
                                    "cost": {
                                        "input": served_rate.load(Ordering::SeqCst),
                                        "output": 4.0
                                    }
                                }
                            }
                        }
                    }))
                }
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().expect("create data directory");
        let pricing = PricingSource::new(data_dir.path())
            .with_source_endpoint(endpoint)
            .with_refresh_interval(TEST_REFRESH_INTERVAL);
        let model = openai_model("changing-model");
        let usage = Usage {
            fresh_input_tokens: Some(500_000),
            ..Usage::default()
        };

        assert_eq!(pricing.estimate_cached(&model, &usage), None);
        assert_eq!(requests.load(Ordering::SeqCst), 0, "cold reads never fetch");

        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        assert_eq!(
            pricing
                .estimate_cached(&model, &usage)
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        input_rate.store(6, Ordering::SeqCst);
        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        let_the_interval_pass(&pricing).await;
        assert_eq!(pricing.estimate_cached(&model, &usage), None);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "overdue reads never refresh"
        );
        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(3.0)
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn a_failed_refresh_leaves_the_cached_table_standing() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let failing = Arc::new(AtomicBool::new(false));
        let should_fail = failing.clone();
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let request_count = request_count.clone();
                let should_fail = should_fail.clone();
                async move {
                    request_count.fetch_add(1, Ordering::SeqCst);
                    if should_fail.load(Ordering::SeqCst) {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    Json(json!({
                        "openai": {
                            "models": {
                                "stale-model": {
                                    "cost": { "input": 2.0, "output": 4.0 }
                                }
                            }
                        }
                    }))
                    .into_response()
                }
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().expect("create data directory");
        let pricing = PricingSource::new(data_dir.path())
            .with_source_endpoint(endpoint.clone())
            .with_refresh_interval(TEST_REFRESH_INTERVAL);
        let model = openai_model("stale-model");
        let usage = Usage {
            fresh_input_tokens: Some(500_000),
            ..Usage::default()
        };

        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        failing.store(true, Ordering::SeqCst);
        let_the_interval_pass(&pricing).await;
        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);

        let restarted = PricingSource::new(data_dir.path())
            .with_source_endpoint(endpoint)
            .with_refresh_interval(TEST_REFRESH_INTERVAL);
        assert_eq!(
            restarted
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn first_run_offline_times_out_to_absent_cost_without_repeated_fetches() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let request_count = request_count.clone();
                async move {
                    request_count.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<StatusCode>().await
                }
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().expect("create data directory");
        let pricing = PricingSource::new(data_dir.path())
            .with_source_endpoint(endpoint)
            .with_fetch_timeout(Duration::from_millis(20));
        let usage = Usage {
            fresh_input_tokens: Some(500_000),
            ..Usage::default()
        };

        assert_eq!(
            pricing
                .estimate(&openai_model("offline-model"), &usage)
                .await,
            None
        );
        assert_eq!(
            pricing
                .estimate(&openai_model("offline-model"), &usage)
                .await,
            None
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(!data_dir.path().join(CACHE_FILE).exists());
        server.abort();
    }

    #[tokio::test]
    async fn a_structurally_malformed_refresh_does_not_replace_the_cached_table() {
        let malformed = Arc::new(AtomicBool::new(false));
        let serve_malformed = malformed.clone();
        let app = Router::new().route(
            "/api.json",
            get(move || {
                let serve_malformed = serve_malformed.clone();
                async move {
                    if serve_malformed.load(Ordering::SeqCst) {
                        Json(json!({ "unexpected": "document" }))
                    } else {
                        Json(json!({
                            "openai": {
                                "models": {
                                    "preserved-model": {
                                        "cost": { "input": 2.0, "output": 4.0 }
                                    }
                                }
                            }
                        }))
                    }
                }
            }),
        );
        let (endpoint, server) = serve_fixture(app).await;
        let data_dir = tempfile::tempdir().expect("create data directory");
        let pricing = PricingSource::new(data_dir.path())
            .with_source_endpoint(endpoint)
            .with_refresh_interval(TEST_REFRESH_INTERVAL);
        let model = openai_model("preserved-model");
        let usage = Usage {
            fresh_input_tokens: Some(500_000),
            ..Usage::default()
        };

        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        malformed.store(true, Ordering::SeqCst);
        let_the_interval_pass(&pricing).await;
        assert_eq!(
            pricing
                .estimate(&model, &usage)
                .await
                .map(EstimatedCost::cost),
            Cost::from_usd(1.0)
        );
        server.abort();
    }
}
