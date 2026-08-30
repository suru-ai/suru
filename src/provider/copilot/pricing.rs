//! Copilot's own Model prices, retained from catalog discovery and applied to
//! ephemeral per-call usage while it is still attributable to a Turn.

use std::collections::HashMap;

use github_copilot_sdk::{rpc::Model, session_events::AssistantUsageData};

use crate::protocol::Cost;

/// Copilot documents each AI credit as one cent. Catalog prices are credits
/// per token batch, so this is the final conversion into the dollar Cost Suru
/// stores.
const USD_PER_AI_CREDIT: f64 = 0.01;

#[derive(Default)]
pub(super) struct CopilotPricing {
    models: HashMap<String, ModelPrices>,
    loaded: bool,
}

impl CopilotPricing {
    pub(super) fn replace(&mut self, models: &[Model]) {
        self.models = models
            .iter()
            .filter_map(|model| {
                ModelPrices::from_model(model).map(|prices| (model.id.clone(), prices))
            })
            .collect();
        self.loaded = true;
    }

    pub(super) fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub(super) fn cost(&self, usage: &AssistantUsageData) -> Option<Cost> {
        self.models.get(&usage.model)?.cost(usage)
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
        let standard = TierPrices {
            input: valid_price(prices.input_price),
            output: valid_price(prices.output_price),
            cache_read: valid_price(prices.cache_read_price),
            cache_write: valid_price(prices.cache_write_price),
        };
        let long_context = prices.long_context.as_ref().map(|long| TierPrices {
            input: valid_price(long.input_price),
            output: valid_price(long.output_price),
            cache_read: valid_price(long.cache_read_price),
            cache_write: valid_price(long.cache_write_price),
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

    fn cost(&self, usage: &AssistantUsageData) -> Option<Cost> {
        let tier = self.tier(usage.max_prompt_tokens);
        let fresh_input = exclusive_count(
            usage.input_tokens,
            [usage.cache_read_tokens, usage.cache_write_tokens],
        )?;
        let cache_read = reported_or_zero(usage.cache_read_tokens)?;
        let cache_write = reported_or_zero(usage.cache_write_tokens)?;
        let output = reported_or_zero(usage.output_tokens)?;
        let credits = component(fresh_input, tier.input)?
            + component(cache_read, tier.cache_read.or(tier.input))?
            + component(cache_write, tier.cache_write.or(tier.input))?
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
}

fn positive_count(count: i64) -> Option<u64> {
    u64::try_from(count).ok().filter(|count| *count > 0)
}

fn valid_price(price: Option<f64>) -> Option<f64> {
    price.filter(|price| price.is_finite() && *price >= 0.0)
}

fn reported_or_zero(count: Option<i64>) -> Option<u64> {
    count.map_or(Some(0), |count| u64::try_from(count).ok())
}

fn exclusive_count(total: Option<i64>, subsets: [Option<i64>; 2]) -> Option<u64> {
    let total = u64::try_from(total?).ok()?;
    subsets.into_iter().try_fold(total, |remaining, subset| {
        remaining.checked_sub(reported_or_zero(subset)?)
    })
}

fn component(tokens: u64, price: Option<f64>) -> Option<f64> {
    if tokens == 0 {
        Some(0.0)
    } else {
        price.map(|price| tokens as f64 * price)
    }
}
