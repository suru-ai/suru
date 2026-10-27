//! Normalization of Copilot's Model catalog onto Suru's [`ModelDescriptor`].
//!
//! Copilot advertises two per-Model configuration dimensions Suru surfaces as Model Options:
//! reasoning effort, from the efforts a Model declares support for, and context tier, which trades
//! context window against cost. A Model that advertises neither carries no Options at all. What
//! else Copilot publishes about a Model — its relative cost, token prices per context tier, limits,
//! and the advisories it asks hosts to surface — becomes the Model's details.

use std::collections::HashSet;

use github_copilot_sdk::{
    rpc::{
        Model, ModelBillingPromo, ModelBillingTokenPrices, ModelMessage, ModelPickerCategory,
        ModelPickerPriceCategory, ModelPolicyState,
    },
    session_events::ContextTier,
};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{CONTEXT_TIER_OPTION_ID, COPILOT_PROVIDER_ID, REASONING_EFFORT_OPTION_ID};
use crate::{
    protocol::{
        ModelAdvisory, ModelAdvisorySeverity, ModelAvailability, ModelDescriptor, ModelDetails,
        ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId,
        ModelOptionKind, ModelOptionRole, ModelPrice, ModelPriceCategory, ModelPriceTier,
        ModelPrices, ProviderId,
    },
    provider::humanized_wire_id,
};

/// The Model Copilot routes on the user's behalf. Copilot's catalog names no default of its own, so
/// when it offers this one Suru defaults to it rather than guessing between concrete Models.
const AUTO_MODEL_ID: &str = "auto";

/// The wire name Copilot knows `tier` by, read out of the SDK's own serialization rather than
/// written here: a Session lowers these same IDs back over the wire, so a tier Copilot renames must
/// arrive renamed rather than quietly going stale.
pub(super) fn tier_id(tier: ContextTier) -> String {
    let Ok(Value::String(id)) = serde_json::to_value(tier) else {
        unreachable!("the SDK serializes a Copilot context tier as its wire name");
    };
    id
}

/// Turns the Models Copilot reports into Suru's catalog, marking exactly one of them the default.
///
/// Each Model stands once, where it first appeared. CLI 1.0.89 lists the catalog once per account
/// it holds credentials for — a `copilot login` beside the gh CLI's credentials is two — as one
/// flat list that says nothing about which account an entry came from, so the copies are told
/// apart by nothing and the first keeps the CLI's own picker order
/// (docs/validation/copilot-model-list-per-account.md).
pub(super) fn model_descriptors(models: Vec<Model>) -> Vec<ModelDescriptor> {
    let mut listed = HashSet::new();
    let mut descriptors = models
        .into_iter()
        .filter(|model| listed.insert(model.id.clone()))
        .map(model_descriptor)
        .collect::<Vec<_>>();
    if let Some(default) = default_model_index(&descriptors) {
        descriptors[default].is_default = true;
    }
    descriptors
}

/// The Model a fresh Agent Selection starts from: Copilot's own routed Model when it is offered and
/// usable, otherwise the first usable Model Copilot listed — its catalog arrives in picker order.
fn default_model_index(descriptors: &[ModelDescriptor]) -> Option<usize> {
    let usable =
        |descriptor: &ModelDescriptor| descriptor.availability == ModelAvailability::Available;
    descriptors
        .iter()
        .position(|descriptor| usable(descriptor) && descriptor.id.as_str() == AUTO_MODEL_ID)
        .or_else(|| descriptors.iter().position(usable))
}

fn model_descriptor(model: Model) -> ModelDescriptor {
    let mut options = Vec::new();
    if let Some(effort) = reasoning_effort_option(&model) {
        options.push(effort);
    }
    if let Some(tier) = context_tier_option(&model) {
        options.push(tier);
    }
    let details = model_details(&model);
    ModelDescriptor {
        provider: ProviderId::new(COPILOT_PROVIDER_ID),
        id: ModelId::new(model.id),
        display_name: model.name,
        description: category_description(model.model_picker_category),
        // Only the catalog as a whole knows which Model is the default.
        is_default: false,
        availability: policy_availability(model.policy.as_ref().map(|policy| &policy.state)),
        options,
        details,
    }
}

/// Copilot publishes no prose about a Model, only the capability class its own picker groups it
/// under, so that class is the whole description.
fn category_description(category: Option<ModelPickerCategory>) -> String {
    match category {
        Some(ModelPickerCategory::Lightweight) => "Lightweight".to_owned(),
        Some(ModelPickerCategory::Versatile) => "Versatile".to_owned(),
        Some(ModelPickerCategory::Powerful) => "Powerful".to_owned(),
        Some(ModelPickerCategory::Unknown) | None => String::new(),
    }
}

/// A Model an administrator's policy disables stays visible but unselectable; every other policy
/// state — including one Copilot grew after this was written — leaves it usable.
fn policy_availability(state: Option<&ModelPolicyState>) -> ModelAvailability {
    match state {
        Some(ModelPolicyState::Disabled) => ModelAvailability::Unavailable,
        _ => ModelAvailability::Available,
    }
}

/// What Copilot publishes about `model` beyond its options.
fn model_details(model: &Model) -> ModelDetails {
    let limits = model.capabilities.limits.as_ref();
    ModelDetails {
        price_category: model
            .model_picker_price_category
            .as_ref()
            .and_then(price_category),
        context_window_tokens: limits
            .and_then(|limits| limits.max_context_window_tokens)
            .and_then(token_count),
        output_limit_tokens: limits
            .and_then(|limits| limits.max_output_tokens)
            .and_then(token_count),
        accepts_images: model
            .capabilities
            .supports
            .as_ref()
            .and_then(|supports| supports.vision),
        prices: model
            .billing
            .as_ref()
            .and_then(|billing| billing.token_prices.as_ref())
            .and_then(token_prices),
        advisories: advisories(model),
    }
}

fn price_category(category: &ModelPickerPriceCategory) -> Option<ModelPriceCategory> {
    match category {
        ModelPickerPriceCategory::Low => Some(ModelPriceCategory::Low),
        ModelPickerPriceCategory::Medium => Some(ModelPriceCategory::Medium),
        ModelPickerPriceCategory::High => Some(ModelPriceCategory::High),
        ModelPickerPriceCategory::VeryHigh => Some(ModelPriceCategory::VeryHigh),
        ModelPickerPriceCategory::Unknown => None,
    }
}

/// A count Copilot reports, where it is a count at all.
pub(super) fn token_count(count: i64) -> Option<u64> {
    u64::try_from(count).ok().filter(|count| *count > 0)
}

/// Copilot's token prices, in the AI Credits it bills, one tier per context tier: the standard
/// tier always, then the extended one where the Model publishes a second price for it. Each tier is
/// labelled as the Model's Context option labels the same tier. Both the catalog and the Costs
/// Suru works out from a Turn's usage read Copilot's prices from here.
pub(super) fn token_prices(prices: &ModelBillingTokenPrices) -> Option<ModelPrices> {
    // Copilot publishes both tiers' prices under the same field names, in types of their own.
    macro_rules! tier {
        ($tier:expr, $prices:expr) => {{
            let prices = $prices;
            let price = |units: Option<f64>| units.and_then(ModelPrice::from_units);
            #[allow(deprecated)]
            let cache_read = prices.cache_read_price.or(prices.cache_price);
            ModelPriceTier {
                label: humanized_wire_id(&tier_id($tier)),
                max_prompt_tokens: prices.max_prompt_tokens.and_then(token_count),
                input: price(prices.input_price),
                output: price(prices.output_price),
                cache_read: price(cache_read),
                cache_write: price(prices.cache_write_price),
                cache_write_1h: price(prices.cache_write1h_price),
            }
        }};
    }
    let mut tiers = vec![tier!(ContextTier::Default, prices)];
    if let Some(long) = &prices.long_context {
        tiers.push(tier!(ContextTier::LongContext, long));
    }
    Some(ModelPrices {
        unit: "AI Credits".to_owned(),
        per_tokens: prices.batch_size.and_then(token_count)?,
        tiers,
    })
}

/// What Copilot asks a host to surface beside `model`, warnings first: the data-retention warning
/// it requires be shown, its warning messages, and why a policy disables the Model; then its
/// informational messages, a promotion still running, and the discount its routed Model bills at.
fn advisories(model: &Model) -> Vec<ModelAdvisory> {
    let advisory = |severity, title: Option<&str>, text: String| ModelAdvisory {
        severity,
        title: title.map(str::to_owned),
        text,
    };
    let messages = |messages: &Option<Vec<ModelMessage>>| {
        messages
            .iter()
            .flatten()
            .map(|message| message.message.clone())
            .collect::<Vec<_>>()
    };
    let mut advisories = Vec::new();
    if let Some(retention) = model
        .warning_text
        .as_ref()
        .and_then(|warning| warning.data_retention.clone())
    {
        advisories.push(advisory(
            ModelAdvisorySeverity::Warning,
            Some("Data retention"),
            retention,
        ));
    }
    for message in messages(&model.warning_messages) {
        advisories.push(advisory(ModelAdvisorySeverity::Warning, None, message));
    }
    if let Some(policy) = &model.policy
        && policy.state == ModelPolicyState::Disabled
    {
        advisories.push(advisory(
            ModelAdvisorySeverity::Warning,
            Some("Disabled by policy"),
            policy.terms.clone().unwrap_or_default(),
        ));
    }
    for message in messages(&model.info_messages) {
        advisories.push(advisory(ModelAdvisorySeverity::Info, None, message));
    }
    let billing = model.billing.as_ref();
    if let Some(text) = billing
        .and_then(|billing| billing.promo.as_ref())
        .and_then(promotion)
    {
        advisories.push(advisory(
            ModelAdvisorySeverity::Info,
            Some("Promotion"),
            text,
        ));
    }
    if let Some(percent) = billing.and_then(|billing| billing.discount_percent) {
        advisories.push(advisory(
            ModelAdvisorySeverity::Info,
            None,
            format!("Billed {percent}% below the price of the Model Copilot picks"),
        ));
    }
    advisories
}

/// A promotion as a reader is told it, with the day it ends. One that has already ended says
/// nothing: Copilot treats a past end as expired, and a remembered catalog outlives the promotion.
fn promotion(promo: &ModelBillingPromo) -> Option<String> {
    let offer = promo.message.clone().or_else(|| {
        promo
            .discount_percent
            .map(|percent| format!("{percent}% off"))
    })?;
    let ends = promo
        .ends_at
        .as_deref()
        .and_then(|ends| OffsetDateTime::parse(ends, &Rfc3339).ok());
    match ends {
        Some(ends) if ends <= OffsetDateTime::now_utc() => None,
        Some(ends) => Some(format!("{offer}, until {}", ends.date())),
        None => Some(offer),
    }
}

fn reasoning_effort_option(model: &Model) -> Option<ModelOptionDescriptor> {
    let efforts = model.supported_reasoning_efforts.as_ref()?;
    let default = preferred_default(efforts, model.default_reasoning_effort.as_deref())?;
    Some(ModelOptionDescriptor {
        id: ModelOptionId::new(REASONING_EFFORT_OPTION_ID),
        label: "Reasoning effort".to_owned(),
        description: None,
        role: ModelOptionRole::ReasoningEffort,
        kind: select_of(efforts, default),
    })
}

/// The context tiers a Model offers, however Copilot publishes them.
fn context_tier_option(model: &Model) -> Option<ModelOptionDescriptor> {
    let tiers = declared_context_tiers(model).or_else(|| priced_context_tiers(model))?;
    let standard = tier_id(ContextTier::Default);
    let default = preferred_default(&tiers, Some(&standard))?;
    Some(ModelOptionDescriptor {
        id: ModelOptionId::new(CONTEXT_TIER_OPTION_ID),
        label: "Context".to_owned(),
        description: None,
        role: ModelOptionRole::Context,
        kind: select_of(&tiers, default),
    })
}

/// The tiers a Model names outright, which is how a Provider with no pricing to publish — an agent
/// host Copilot reaches over its own protocol — declares them.
fn declared_context_tiers(model: &Model) -> Option<Vec<String>> {
    model
        .supported_context_tiers
        .as_ref()
        .filter(|tiers| !tiers.is_empty())
        .cloned()
}

/// The tiers implied by tiered token pricing, which is how Copilot's own Models carry them: an
/// extended tier is a second price rather than a named tier.
fn priced_context_tiers(model: &Model) -> Option<Vec<String>> {
    model
        .billing
        .as_ref()?
        .token_prices
        .as_ref()?
        .long_context
        .as_ref()?;
    Some(vec![
        tier_id(ContextTier::Default),
        tier_id(ContextTier::LongContext),
    ])
}

/// The choice a Select Model Option defaults to: the one the Provider named when it is really on
/// offer, and the first offered choice otherwise, because a default that is not a choice would
/// reject the whole catalog.
fn preferred_default<'a>(choices: &'a [String], preferred: Option<&'a str>) -> Option<&'a str> {
    let named = preferred.filter(|preferred| choices.iter().any(|choice| choice == preferred));
    named.or_else(|| choices.first().map(String::as_str))
}

/// A Select Model Option over `choices`, each labelled for reading.
fn select_of(choices: &[String], default: &str) -> ModelOptionKind {
    ModelOptionKind::Select {
        choices: choices
            .iter()
            .map(|choice| ModelOptionChoice {
                label: humanized_wire_id(choice),
                id: ModelOptionChoiceId::new(choice.clone()),
                description: None,
                availability: ModelAvailability::Available,
            })
            .collect(),
        default: ModelOptionChoiceId::new(default),
    }
}
