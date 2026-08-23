//! Normalization of Copilot's Model catalog onto Suru's [`ModelDescriptor`].
//!
//! Copilot advertises two per-Model configuration dimensions Suru surfaces as Model Options:
//! reasoning effort, from the efforts a Model declares support for, and context tier, which trades
//! context window against cost. A Model that advertises neither carries no Options at all.

use github_copilot_sdk::rpc::{Model, ModelPickerCategory, ModelPolicyState};

use super::{CONTEXT_TIER_OPTION_ID, DEFAULT_CONTEXT_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID};
use crate::protocol::{
    ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId,
    ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole, ProviderId,
};

/// The Model Copilot routes on the user's behalf. Copilot's catalog names no default of its own, so
/// when it offers this one Suru defaults to it rather than guessing between concrete Models.
const AUTO_MODEL_ID: &str = "auto";

/// The extended context tier, which Copilot's own Models publish as a second set of token prices
/// rather than by name.
const LONG_CONTEXT_TIER_CHOICE_ID: &str = "long_context";

/// Turns the Models Copilot reports into Suru's catalog, marking exactly one of them the default.
pub(super) fn model_descriptors(models: Vec<Model>) -> Vec<ModelDescriptor> {
    let mut descriptors = models.into_iter().map(model_descriptor).collect::<Vec<_>>();
    if let Some(default) = default_model_index(&descriptors) {
        descriptors[default].is_default = true;
    }
    descriptors
}

/// The Model a fresh Agent Selection starts from: Copilot's own routed Model when it is offered and
/// usable, otherwise the first usable Model Copilot listed — its catalog arrives in picker order.
fn default_model_index(descriptors: &[ModelDescriptor]) -> Option<usize> {
    let mut usable = descriptors
        .iter()
        .enumerate()
        .filter(|(_, descriptor)| descriptor.availability == ModelAvailability::Available);
    let (first, _) = usable.next()?;
    let routed = usable
        .find(|(_, descriptor)| descriptor.id.as_str() == AUTO_MODEL_ID)
        .map(|(index, _)| index);
    Some(routed.unwrap_or(first))
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
        provider: ProviderId::new("copilot"),
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
        kind: select(efforts, default),
    })
}

/// The context tiers a Model offers, however Copilot publishes them.
fn context_tier_option(model: &Model) -> Option<ModelOptionDescriptor> {
    let tiers = declared_context_tiers(model).or_else(|| priced_context_tiers(model))?;
    let default = preferred_default(&tiers, Some(DEFAULT_CONTEXT_TIER_CHOICE_ID))?;
    Some(ModelOptionDescriptor {
        id: ModelOptionId::new(CONTEXT_TIER_OPTION_ID),
        label: "Context".to_owned(),
        description: None,
        role: ModelOptionRole::Context,
        kind: select(&tiers, default),
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
        DEFAULT_CONTEXT_TIER_CHOICE_ID.to_owned(),
        LONG_CONTEXT_TIER_CHOICE_ID.to_owned(),
    ])
}

/// The choice a Select Model Option defaults to: the one the Provider named when it is really on
/// offer, and the first offered choice otherwise, because a default that is not a choice would
/// reject the whole catalog.
fn preferred_default<'a>(choices: &'a [String], preferred: Option<&'a str>) -> Option<&'a str> {
    let named = preferred.filter(|preferred| choices.iter().any(|choice| choice == preferred));
    named.or_else(|| choices.first().map(String::as_str))
}

fn select(choices: &[String], default: &str) -> ModelOptionKind {
    ModelOptionKind::Select {
        choices: choices
            .iter()
            .map(|choice| ModelOptionChoice {
                label: humanize_id(choice),
                id: ModelOptionChoiceId::new(choice.clone()),
                description: None,
                availability: ModelAvailability::Available,
            })
            .collect(),
        default: ModelOptionChoiceId::new(default),
    }
}

/// Copilot names its choices in wire case (`long_context`); this is the reading version.
fn humanize_id(value: &str) -> String {
    let spaced = value.replace(['_', '-'], " ");
    let mut characters = spaced.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}
