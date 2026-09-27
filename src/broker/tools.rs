//! The Tools the Broker offers, each described once and answered in Suru's
//! own terms. Nothing here knows MCP: the transport in [`super::mcp`] turns a
//! [`BrokerTool`]'s description into what `tools/list` lists and routes a
//! `tools/call` into [`BrokerTools::call`].

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::BrokerCaller;
use crate::{
    model_catalog::ModelCatalogService,
    protocol::{
        ModelAvailability, ModelCatalog, ModelDescriptor, ModelId, ModelOptionChoiceId,
        ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ProviderCatalogStatus, ProviderId,
        ProviderModelCatalog, ProviderUnavailability,
    },
};

/// One Tool the Broker offers. A new Tool is a variant here, its description
/// beside the others, and an arm of [`BrokerTools::call`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrokerTool {
    ListProviders,
}

impl BrokerTool {
    /// Every Tool, in the order `tools/list` lists them.
    pub(super) const ALL: [Self; 1] = [Self::ListProviders];

    pub(super) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::ListProviders => "list_providers",
        }
    }

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::ListProviders => "List Providers",
        }
    }

    /// What the calling Agent reads about the Tool: when to use it, what it
    /// takes, and the shape of what it answers — which is stable, so an Agent
    /// may rely on it.
    pub(super) fn description(self) -> &'static str {
        match self {
            Self::ListProviders => LIST_PROVIDERS_DESCRIPTION,
        }
    }

    /// The JSON Schema of the Tool's arguments.
    pub(super) fn input_schema(self) -> Map<String, Value> {
        let schema = match self {
            Self::ListProviders => json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        };
        let Value::Object(schema) = schema else {
            unreachable!("every input schema is a JSON object");
        };
        schema
    }

    /// Whether the Tool only reads, changing nothing Suru holds.
    pub(super) fn is_read_only(self) -> bool {
        match self {
            Self::ListProviders => true,
        }
    }
}

const LIST_PROVIDERS_DESCRIPTION: &str = "\
List the Providers Suru hosts, with the Models and Model Options each offers, \
so you can choose where to delegate work. Takes no arguments. Answers with \
JSON of the shape {\"providers\": [provider, ...]}, one entry per hosted \
Provider in Suru's order. Each provider has \"id\"; \"name\"; \"enabled\" \
(false when the user has turned the Provider off in Suru); \"available\" (true \
when it can be used now, false when it cannot, null when it is turned off and \
so was never checked); \"reason\" and \"detail\" when \"available\" is false, \
where reason is one of \"not_installed\", \"not_signed_in\", \
\"incompatible_version\", \"catalog_failed\" or \"checking\" and detail says \
what to tell the user; and \"models\", empty unless the Provider is available. \
Each model has \"id\", \"name\", \"description\", \"default\" (true for the \
Model the Provider uses unless told otherwise) and \"options\". Each Model \
Option has \"id\", \"name\" and \"type\": a \"select\" option has \"choices\" \
(each with \"id\" and \"name\") and the \"default\" choice id; a \"toggle\" \
option has a boolean \"default\". Only Models and choices that can be selected \
are listed. Availability is as Suru last found it; calling again does not \
re-check a Provider.";

/// One call of a Tool: who is calling, and the arguments as the Agent sent
/// them.
pub(super) struct ToolCall {
    pub(super) caller: BrokerCaller,
    pub(super) arguments: Map<String, Value>,
}

/// Why a Tool refused a call, in words the calling Agent reads.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ToolRefusal(String);

impl std::fmt::Display for ToolRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The Server-side services the Broker's Tools answer from.
#[derive(Clone)]
pub(crate) struct BrokerTools {
    model_catalog: ModelCatalogService,
}

impl BrokerTools {
    pub(crate) fn new(model_catalog: ModelCatalogService) -> Self {
        Self { model_catalog }
    }

    /// Answers one call of `tool` with the JSON its description promises.
    pub(super) async fn call(
        &self,
        tool: BrokerTool,
        call: ToolCall,
    ) -> Result<Value, ToolRefusal> {
        tracing::debug!(
            tool = tool.name(),
            session_id = %call.caller.session_id(),
            "Broker Tool called"
        );
        match tool {
            BrokerTool::ListProviders => {
                takes_no_arguments(tool, &call.arguments)?;
                Ok(
                    serde_json::to_value(provider_listing(self.model_catalog.known().await))
                        .expect("a Provider listing always serializes"),
                )
            }
        }
    }
}

fn takes_no_arguments(tool: BrokerTool, arguments: &Map<String, Value>) -> Result<(), ToolRefusal> {
    if arguments.is_empty() {
        return Ok(());
    }
    let named = arguments.keys().cloned().collect::<Vec<_>>().join(", ");
    Err(ToolRefusal(format!(
        "{} takes no arguments; call it again without {named}.",
        tool.name()
    )))
}

/// What `list_providers` answers.
#[derive(Debug, Serialize)]
struct ProviderListing {
    providers: Vec<ListedProvider>,
}

#[derive(Debug, Serialize)]
struct ListedProvider {
    id: ProviderId,
    name: String,
    enabled: bool,
    /// `None` for a Provider the user turned off: Suru never checked it, so
    /// there is no Availability to report.
    available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    models: Vec<ListedModel>,
}

#[derive(Debug, Serialize)]
struct ListedModel {
    id: ModelId,
    name: String,
    description: String,
    default: bool,
    options: Vec<ListedOption>,
}

#[derive(Debug, Serialize)]
struct ListedOption {
    id: ModelOptionId,
    name: String,
    #[serde(flatten)]
    kind: ListedOptionKind,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ListedOptionKind {
    Select {
        choices: Vec<ListedChoice>,
        default: ModelOptionChoiceId,
    },
    Toggle {
        default: bool,
    },
}

#[derive(Debug, Serialize)]
struct ListedChoice {
    id: ModelOptionChoiceId,
    name: String,
}

/// The Model Catalog as an Agent choosing where to delegate reads it: every
/// hosted Provider, whether the user has it on, whether it can be used now and
/// why not, and — only where it can — the Models and Model Option choices that
/// may be selected. A Model or choice the Provider lists but will not run is
/// left out, since naming it could only be refused.
fn provider_listing(catalog: ModelCatalog) -> ProviderListing {
    ProviderListing {
        providers: catalog.providers.into_iter().map(listed_provider).collect(),
    }
}

fn listed_provider(catalog: ProviderModelCatalog) -> ListedProvider {
    let ProviderModelCatalog {
        provider,
        display_name,
        models,
        status,
    } = catalog;
    let listed = |available, reason, detail, models| ListedProvider {
        id: provider.clone(),
        name: display_name.clone(),
        enabled: !matches!(status, ProviderCatalogStatus::Disabled),
        available,
        reason,
        detail,
        models,
    };
    match &status {
        ProviderCatalogStatus::Disabled => listed(None, None, None, Vec::new()),
        ProviderCatalogStatus::Unavailable { reason, message } => listed(
            Some(false),
            Some(unavailability_reason(*reason)),
            Some(message.clone()),
            Vec::new(),
        ),
        ProviderCatalogStatus::Failed { message } => listed(
            Some(false),
            Some("catalog_failed"),
            Some(message.clone()),
            Vec::new(),
        ),
        // A first discovery still in flight has nothing to offer yet; a
        // re-check of Models already known leaves them selectable.
        ProviderCatalogStatus::Refreshing if models.is_empty() => listed(
            Some(false),
            Some("checking"),
            Some(
                "Suru is still asking this Provider for its Models; ask again shortly.".to_owned(),
            ),
            Vec::new(),
        ),
        ProviderCatalogStatus::Fresh
        | ProviderCatalogStatus::Warning { .. }
        | ProviderCatalogStatus::Stale { .. }
        | ProviderCatalogStatus::Refreshing => listed(
            Some(true),
            None,
            None,
            models
                .iter()
                .filter(|model| model.availability == ModelAvailability::Available)
                .map(listed_model)
                .collect(),
        ),
    }
}

fn unavailability_reason(reason: ProviderUnavailability) -> &'static str {
    match reason {
        ProviderUnavailability::NotInstalled => "not_installed",
        ProviderUnavailability::NotSignedIn => "not_signed_in",
        ProviderUnavailability::IncompatibleVersion => "incompatible_version",
    }
}

fn listed_model(model: &ModelDescriptor) -> ListedModel {
    ListedModel {
        id: model.id.clone(),
        name: model.display_name.clone(),
        description: model.description.clone(),
        default: model.is_default,
        options: model.options.iter().map(listed_option).collect(),
    }
}

fn listed_option(option: &ModelOptionDescriptor) -> ListedOption {
    ListedOption {
        id: option.id.clone(),
        name: option.label.clone(),
        kind: match &option.kind {
            ModelOptionKind::Select { choices, default } => ListedOptionKind::Select {
                choices: choices
                    .iter()
                    .filter(|choice| choice.availability == ModelAvailability::Available)
                    .map(|choice| ListedChoice {
                        id: choice.id.clone(),
                        name: choice.label.clone(),
                    })
                    .collect(),
                default: default.clone(),
            },
            ModelOptionKind::Toggle { default } => ListedOptionKind::Toggle { default: *default },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ModelOptionChoice, ModelOptionRole};

    fn catalog(status: ProviderCatalogStatus, models: Vec<ModelDescriptor>) -> ModelCatalog {
        ModelCatalog {
            providers: vec![ProviderModelCatalog {
                provider: ProviderId::new("codex"),
                display_name: "Codex".to_owned(),
                models,
                status,
            }],
        }
    }

    fn listed(status: ProviderCatalogStatus, models: Vec<ModelDescriptor>) -> Value {
        serde_json::to_value(provider_listing(catalog(status, models)))
            .expect("a Provider listing serializes")["providers"][0]
            .clone()
    }

    fn model(id: &str, availability: ModelAvailability) -> ModelDescriptor {
        let choice = |id: &str, availability| ModelOptionChoice {
            id: ModelOptionChoiceId::new(id),
            label: id.to_uppercase(),
            description: Some("not listed".to_owned()),
            availability,
        };
        ModelDescriptor {
            provider: ProviderId::new("codex"),
            id: ModelId::new(id),
            display_name: id.to_uppercase(),
            description: String::new(),
            is_default: false,
            availability,
            options: vec![ModelOptionDescriptor {
                id: ModelOptionId::new("effort"),
                label: "Effort".to_owned(),
                description: Some("not listed".to_owned()),
                role: ModelOptionRole::ReasoningEffort,
                kind: ModelOptionKind::Select {
                    choices: vec![
                        choice("low", ModelAvailability::Available),
                        choice("max", ModelAvailability::Unavailable),
                    ],
                    default: ModelOptionChoiceId::new("low"),
                },
            }],
        }
    }

    #[test]
    fn only_the_models_and_choices_a_provider_will_run_are_offered() {
        let listed = listed(
            ProviderCatalogStatus::Fresh,
            vec![
                model("runs", ModelAvailability::Available),
                model("refused", ModelAvailability::Unavailable),
            ],
        );
        assert_eq!(
            listed["models"],
            json!([{
                "id": "runs",
                "name": "RUNS",
                "description": "",
                "default": false,
                "options": [{
                    "id": "effort",
                    "name": "Effort",
                    "type": "select",
                    "choices": [{ "id": "low", "name": "LOW" }],
                    "default": "low",
                }],
            }])
        );
    }

    #[test]
    fn a_provider_with_known_models_is_available_whatever_its_catalog_is_doing() {
        for status in [
            ProviderCatalogStatus::Fresh,
            ProviderCatalogStatus::Warning {
                message: "update soon".to_owned(),
            },
            ProviderCatalogStatus::Stale {
                message: "the last check failed".to_owned(),
            },
            ProviderCatalogStatus::Refreshing,
        ] {
            let listed = listed(
                status.clone(),
                vec![model("runs", ModelAvailability::Available)],
            );
            assert_eq!(listed["available"], json!(true), "{status:?}");
            assert_eq!(listed["enabled"], json!(true), "{status:?}");
            assert_eq!(listed["models"].as_array().map(Vec::len), Some(1));
            assert!(listed.get("reason").is_none(), "{status:?}");
        }
    }

    #[test]
    fn a_provider_that_cannot_be_used_says_why_and_offers_nothing() {
        let known = vec![model("runs", ModelAvailability::Available)];
        for (status, models, reason, detail) in [
            (
                ProviderCatalogStatus::Unavailable {
                    reason: ProviderUnavailability::NotInstalled,
                    message: "install the Codex CLI".to_owned(),
                },
                known.clone(),
                "not_installed",
                "install the Codex CLI",
            ),
            (
                ProviderCatalogStatus::Unavailable {
                    reason: ProviderUnavailability::IncompatibleVersion,
                    message: "upgrade the Codex CLI".to_owned(),
                },
                Vec::new(),
                "incompatible_version",
                "upgrade the Codex CLI",
            ),
            (
                ProviderCatalogStatus::Failed {
                    message: "model/list timed out".to_owned(),
                },
                Vec::new(),
                "catalog_failed",
                "model/list timed out",
            ),
        ] {
            let listed = listed(status.clone(), models);
            assert_eq!(listed["available"], json!(false), "{status:?}");
            assert_eq!(listed["reason"], json!(reason), "{status:?}");
            assert_eq!(listed["detail"], json!(detail), "{status:?}");
            assert_eq!(listed["models"], json!([]), "{status:?}");
        }

        let checking = listed(ProviderCatalogStatus::Refreshing, Vec::new());
        assert_eq!(checking["available"], json!(false));
        assert_eq!(checking["reason"], json!("checking"));
    }

    #[test]
    fn a_disabled_provider_reports_no_availability_because_it_was_never_checked() {
        assert_eq!(
            listed(ProviderCatalogStatus::Disabled, Vec::new()),
            json!({
                "id": "codex",
                "name": "Codex",
                "enabled": false,
                "available": null,
                "models": [],
            })
        );
    }

    #[test]
    fn every_tool_is_found_by_the_name_it_is_listed_under() {
        for tool in BrokerTool::ALL {
            assert_eq!(BrokerTool::named(tool.name()), Some(tool));
            assert_eq!(tool.input_schema()["type"], json!("object"));
        }
        assert_eq!(BrokerTool::named("spawn_everything"), None);
    }
}
