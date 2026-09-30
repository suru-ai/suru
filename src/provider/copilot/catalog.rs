//! Normalization of Copilot's Model catalog onto Suru's [`ModelDescriptor`].
//!
//! Copilot advertises two per-Model configuration dimensions Suru surfaces as Model Options:
//! reasoning effort, from the efforts a Model declares support for, and context tier, which trades
//! context window against cost. A Model that advertises neither carries no Options at all.

use std::collections::HashSet;

use github_copilot_sdk::{
    rpc::{Model, ModelPickerCategory, ModelPolicyState},
    session_events::ContextTier,
};
use serde_json::Value;

use super::{CONTEXT_TIER_OPTION_ID, COPILOT_PROVIDER_ID, REASONING_EFFORT_OPTION_ID};
use crate::{
    protocol::{
        ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId,
        ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole, ProviderId,
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
    ModelDescriptor {
        provider: ProviderId::new(COPILOT_PROVIDER_ID),
        id: ModelId::new(model.id),
        display_name: model.name,
        description: category_description(model.model_picker_category),
        // Only the catalog as a whole knows which Model is the default.
        is_default: false,
        availability: policy_availability(model.policy.as_ref().map(|policy| &policy.state)),
        options,
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
