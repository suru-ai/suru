//! Copilot's own Model prices, retained from catalog discovery and applied to
//! ephemeral per-call usage while it is still attributable to a Turn.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use github_copilot_sdk::rpc::Model;

use super::catalog::{token_count, token_prices};
use crate::{
    pricing::CacheRates,
    protocol::{Cost, ModelPrice, ModelPriceTier, Usage},
};

/// Copilot documents each AI credit as one cent. Catalog prices are credits
/// per token batch, so this is the final conversion into the dollar Cost Suru
/// stores.
const USD_PER_AI_CREDIT: f64 = 0.01;

#[derive(Clone, Default)]
pub(super) struct CopilotPricing {
    table: Arc<Mutex<PriceTable>>,
}

#[derive(Default)]
struct PriceTable {
    models: HashMap<String, PriceSchedule>,
    loaded: bool,
}

impl CopilotPricing {
    pub(super) fn replace(&self, models: &[Model]) {
        let mut table = self
            .table
            .lock()
            .expect("Copilot pricing lock is not poisoned");
        table.models = models
            .iter()
            .filter_map(|model| {
                PriceSchedule::from_model(model).map(|prices| (model.id.clone(), prices))
            })
            .collect();
        table.loaded = true;
    }

    pub(super) fn is_loaded(&self) -> bool {
        self.table
            .lock()
            .expect("Copilot pricing lock is not poisoned")
            .loaded
    }

    /// The rates Copilot's catalog prices `model` at: see
    /// [`PriceSchedule::cache_rates`]. Absent where it lists no price for
    /// fresh input, or no catalog has loaded yet.
    pub(super) fn cache_rates(&self, model: &str) -> Option<CacheRates> {
        self.table
            .lock()
            .expect("Copilot pricing lock is not poisoned")
            .models
            .get(model)?
            .cache_rates()
    }

    pub(super) fn cost(
        &self,
        model: &str,
        max_prompt_tokens: Option<i64>,
        cache_ttl_seconds: Option<i64>,
        usage: &Usage,
    ) -> Option<Cost> {
        self.table
            .lock()
            .expect("Copilot pricing lock is not poisoned")
            .models
            .get(model)?
            .cost(max_prompt_tokens, cache_ttl_seconds, usage)
    }
}

/// One Model's prices as a Cost is worked out from them.
struct PriceSchedule {
    batch_size: u64,
    standard: TierPrices,
    long_context: Option<TierPrices>,
    standard_max_prompt_tokens: Option<u64>,
    long_context_max_prompt_tokens: Option<u64>,
}

impl PriceSchedule {
    /// The schedule of the prices the catalog lists for `model`: its standard tier first, then
    /// its long-context tier where it has one.
    fn from_model(model: &Model) -> Option<Self> {
        let prices = token_prices(model.billing.as_ref()?.token_prices.as_ref()?)?;
        let mut tiers = prices.tiers.iter();
        let standard = tiers.next()?;
        let long_context = tiers.next();
        Some(Self {
            batch_size: prices.per_tokens,
            standard: TierPrices::from(standard),
            long_context: long_context.map(TierPrices::from),
            standard_max_prompt_tokens: standard.max_prompt_tokens,
            long_context_max_prompt_tokens: long_context.and_then(|tier| tier.max_prompt_tokens),
        })
    }

    /// The standard tier's input rates in USD per million tokens, as a cache
    /// saving is estimated at: a saving is stated for no particular prompt
    /// size, so the long-context tier is passed over, and the cache-write
    /// price is the default five-minute one, the hour-long write price set
    /// aside. Absent where the tier lists no fresh-input price.
    fn cache_rates(&self) -> Option<CacheRates> {
        let tier = &self.standard;
        Some(CacheRates {
            input: self.usd_per_million(tier.input?),
            cache_read: tier.cache_read.map(|price| self.usd_per_million(price)),
            cache_write: tier.cache_write.map(|price| self.usd_per_million(price)),
        })
    }

    /// `credits` a batch of tokens in dollars a million tokens.
    fn usd_per_million(&self, credits: f64) -> f64 {
        self.usd(credits) * 1_000_000.0
    }

    /// `credits` a batch of tokens in dollars a token.
    fn usd(&self, credits: f64) -> f64 {
        credits / self.batch_size as f64 * USD_PER_AI_CREDIT
    }

    fn cost(
        &self,
        max_prompt_tokens: Option<i64>,
        cache_ttl_seconds: Option<i64>,
        usage: &Usage,
    ) -> Option<Cost> {
        let tier = self.tier(max_prompt_tokens);
        let fresh_input = usage.fresh_input_tokens?;
        let cache_read = usage.cache_read_tokens.unwrap_or(0);
        let cache_write = usage.cache_write_tokens.unwrap_or(0);
        let output = sum_reported(usage.output_tokens, usage.reasoning_tokens)?;
        let credits = component(fresh_input, tier.input)?
            + component(cache_read, tier.cache_read)?
            + component(cache_write, tier.cache_write_price(cache_ttl_seconds))?
            + component(output, tier.output)?;
        Cost::from_usd(self.usd(credits))
    }

    fn tier(&self, max_prompt_tokens: Option<i64>) -> &TierPrices {
        let reported = max_prompt_tokens.and_then(token_count);
        let uses_long_context = reported.is_some_and(|reported| {
            self.long_context_max_prompt_tokens == Some(reported)
                || self
                    .standard_max_prompt_tokens
                    .is_some_and(|standard| reported > standard)
        });
        if uses_long_context {
            self.long_context.as_ref().unwrap_or(&self.standard)
        } else {
            &self.standard
        }
    }
}

struct TierPrices {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    cache_write_1h: Option<f64>,
}

impl From<&ModelPriceTier> for TierPrices {
    fn from(tier: &ModelPriceTier) -> Self {
        let units = |price: Option<ModelPrice>| price.map(ModelPrice::as_units);
        Self {
            input: units(tier.input),
            output: units(tier.output),
            cache_read: units(tier.cache_read),
            cache_write: units(tier.cache_write),
            cache_write_1h: units(tier.cache_write_1h),
        }
    }
}

impl TierPrices {
    fn cache_write_price(&self, cache_ttl_seconds: Option<i64>) -> Option<f64> {
        match cache_ttl_seconds {
            Some(3_600) => self.cache_write_1h,
            None | Some(0..=3_599) => self.cache_write,
            Some(_) => None,
        }
    }
}

fn sum_reported(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => left.checked_add(right),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => Some(0),
    }
}

fn component(tokens: u64, price: Option<f64>) -> Option<f64> {
    if tokens == 0 {
        Some(0.0)
    } else {
        price.map(|price| tokens as f64 * price)
    }
}
