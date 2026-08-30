//! Copilot's own Model prices, retained from catalog discovery and applied to
//! ephemeral per-call usage while it is still attributable to a Turn.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use github_copilot_sdk::rpc::Model;

use crate::protocol::{Cost, Usage};

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
    models: HashMap<String, ModelPrices>,
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
                ModelPrices::from_model(model).map(|prices| (model.id.clone(), prices))
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

struct ModelPrices {
    batch_size: u64,
    standard: TierPrices,
    long_context: Option<TierPrices>,
    standard_max_prompt_tokens: Option<u64>,
    long_context_max_prompt_tokens: Option<u64>,
}

impl ModelPrices {
    fn from_model(model: &Model) -> Option<Self> {
        let prices = model.billing.as_ref()?.token_prices.as_ref()?;
        let batch_size = positive_count(prices.batch_size?)?;
        #[allow(deprecated)]
        let legacy_cache_read_price = prices.cache_price;
        let standard = TierPrices {
            input: valid_price(prices.input_price),
            output: valid_price(prices.output_price),
            cache_read: valid_price(prices.cache_read_price.or(legacy_cache_read_price)),
            cache_write: valid_price(prices.cache_write_price),
            cache_write_1h: valid_price(prices.cache_write1h_price),
        };
        let long_context = prices.long_context.as_ref().map(|long| {
            #[allow(deprecated)]
            let legacy_cache_read_price = long.cache_price;
            TierPrices {
                input: valid_price(long.input_price),
                output: valid_price(long.output_price),
                cache_read: valid_price(long.cache_read_price.or(legacy_cache_read_price)),
                cache_write: valid_price(long.cache_write_price),
                cache_write_1h: valid_price(long.cache_write1h_price),
            }
        });
        Some(Self {
            batch_size,
            standard,
            long_context,
            standard_max_prompt_tokens: prices.max_prompt_tokens.and_then(positive_count),
            long_context_max_prompt_tokens: prices
                .long_context
                .as_ref()
                .and_then(|long| long.max_prompt_tokens)
                .and_then(positive_count),
        })
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
        Cost::from_usd(credits / self.batch_size as f64 * USD_PER_AI_CREDIT)
    }

    fn tier(&self, max_prompt_tokens: Option<i64>) -> &TierPrices {
        let reported = max_prompt_tokens.and_then(positive_count);
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

impl TierPrices {
    fn cache_write_price(&self, cache_ttl_seconds: Option<i64>) -> Option<f64> {
        match cache_ttl_seconds {
            Some(3_600) => self.cache_write_1h,
            None | Some(0..=3_599) => self.cache_write,
            Some(_) => None,
        }
    }
}

fn positive_count(count: i64) -> Option<u64> {
    u64::try_from(count).ok().filter(|count| *count > 0)
}

fn valid_price(price: Option<f64>) -> Option<f64> {
    price.filter(|price| price.is_finite() && *price >= 0.0)
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
