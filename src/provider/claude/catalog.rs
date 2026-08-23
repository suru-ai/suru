//! Presentation of the Claude Code CLI's model rows as Suru's [`ModelDescriptor`]s.
//!
//! The CLI is the source of truth, so rows are presented verbatim: the alias `value` a spawn's
//! model flag accepts is the Model ID, and the CLI's own display names and descriptions ride along
//! unchanged — even when two alias rows resolve to the same canonical model. The only Model Option
//! is a reasoning-effort select built from the effort levels a row advertises; a row without
//! effort metadata carries no Options at all.

use super::{CLAUDE_PROVIDER_ID, REASONING_EFFORT_OPTION_ID, wire::NativeModel};
use crate::{
    protocol::{
        ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId,
        ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole, ProviderId,
    },
    provider::humanized_wire_id,
};

/// The row the CLI's picker recommends. The wire marks no row as the default, but this alias is
/// the CLI's own "Default (recommended)" entry, so when it is offered Suru defaults to it.
const DEFAULT_MODEL_VALUE: &str = "default";

/// The effort the CLI itself runs with when the user has chosen none. The wire names no default
/// effort per row, so Suru mirrors the CLI's default when the row offers it and falls back to the
/// row's first level otherwise.
const DEFAULT_EFFORT_LEVEL: &str = "high";

/// Turns the rows the CLI reports into Suru's catalog, marking exactly one of them the default.
pub(super) fn model_descriptors(models: Vec<NativeModel>) -> Vec<ModelDescriptor> {
    let mut descriptors = models.into_iter().map(model_descriptor).collect::<Vec<_>>();
    let default = descriptors
        .iter()
        .position(|descriptor| descriptor.id.as_str() == DEFAULT_MODEL_VALUE)
        .or(if descriptors.is_empty() { None } else { Some(0) });
    if let Some(default) = default {
        descriptors[default].is_default = true;
    }
    descriptors
}

fn model_descriptor(model: NativeModel) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(CLAUDE_PROVIDER_ID),
        id: ModelId::new(model.value),
        display_name: model.display_name,
        description: model.description,
        // Only the catalog as a whole knows which row is the default.
        is_default: false,
        availability: ModelAvailability::Available,
        options: reasoning_effort_option(&model.supported_effort_levels)
            .into_iter()
            .collect(),
    }
}

fn reasoning_effort_option(levels: &[String]) -> Option<ModelOptionDescriptor> {
    let default = levels
        .iter()
        .find(|level| *level == DEFAULT_EFFORT_LEVEL)
        .or_else(|| levels.first())?;
    Some(ModelOptionDescriptor {
        id: ModelOptionId::new(REASONING_EFFORT_OPTION_ID),
        label: "Reasoning effort".to_owned(),
        description: None,
        role: ModelOptionRole::ReasoningEffort,
        kind: ModelOptionKind::Select {
            choices: levels
                .iter()
                .map(|level| ModelOptionChoice {
                    label: humanized_wire_id(level),
                    id: ModelOptionChoiceId::new(level.clone()),
                    description: None,
                    availability: ModelAvailability::Available,
                })
                .collect(),
            default: ModelOptionChoiceId::new(default.clone()),
        },
    })
}
