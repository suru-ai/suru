//! The reasoning-effort cycle binding and its optimistic selection overlay.

use crate::support::{
    model_descriptor, rendered_application_rows, rendered_application_rows_at,
    selected_session_snapshot, type_terminal_text, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        AgentSelection, AgentSelectionOperationId, ModelAvailability, ModelCatalog,
        ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor,
        ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionValue, ProviderCatalogStatus,
        ProviderId, ProviderModelCatalog, SessionChange, SessionId, SessionRevision, SessionUpdate,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, command_for_terminal_event,
    },
};

fn cycling_choice(id: &str, label: &str, availability: ModelAvailability) -> ModelOptionChoice {
    ModelOptionChoice {
        id: ModelOptionChoiceId::new(id),
        label: label.to_owned(),
        description: None,
        availability,
    }
}

fn cycling_model(choices: Vec<ModelOptionChoice>, default: &str) -> ModelDescriptor {
    let mut model = model_descriptor(
        "codex",
        "gpt-cycle",
        "Cycling GPT",
        true,
        ModelAvailability::Available,
    );
    model.options = vec![
        ModelOptionDescriptor {
            id: ModelOptionId::new("reasoning_effort"),
            label: "Reasoning".to_owned(),
            description: None,
            role: ModelOptionRole::ReasoningEffort,
            kind: ModelOptionKind::Select {
                choices,
                default: ModelOptionChoiceId::new(default),
            },
        },
        ModelOptionDescriptor {
            id: ModelOptionId::new("fast"),
            label: "Fast".to_owned(),
            description: None,
            role: ModelOptionRole::Speed,
            kind: ModelOptionKind::Toggle { default: false },
        },
    ];
    model
}

fn cycling_effort_ladder() -> Vec<ModelOptionChoice> {
    vec![
        cycling_choice("low", "Low", ModelAvailability::Available),
        cycling_choice("medium", "Medium", ModelAvailability::Available),
        cycling_choice("high", "High", ModelAvailability::Available),
    ]
}

fn cycling_selection(effort: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-cycle"),
        options: vec![
            suru::protocol::ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(effort),
                },
            },
            suru::protocol::ModelOptionSelection {
                id: ModelOptionId::new("fast"),
                value: ModelOptionValue::Toggle { enabled: false },
            },
        ],
    }
}

fn warm_model_catalog(application: &mut Application, model: ModelDescriptor) {
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker to warm the catalog")
    else {
        panic!("Model picker should request the catalog");
    };
    let provider = model.provider.clone();
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    display_name: provider.to_string(),
                    provider,
                    models: vec![model],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("cache the Model catalog");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close the warmed Model picker");
}

fn press_reasoning_cycle(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+T")
}

fn reasoning_summary(application: &Application) -> String {
    rendered_application_rows_at(application, 100, 16).join("\n")
}

fn assert_reasoning_summary(application: &Application, effort: &str) {
    let summary = reasoning_summary(application);
    assert_reasoning_summary_text(&summary, effort);
}

fn assert_reasoning_summary_text(summary: &str, effort: &str) {
    let expected = format!("codex · Cycling GPT · {effort} · Off");
    assert!(
        summary.contains(&expected),
        "the footer omitted {expected:?}: {summary}"
    );
}

#[test]
fn ctrl_t_dispatches_one_semantic_reasoning_cycle_without_a_slash_name() {
    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        ))),
        Some(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelOptionReasoningCycle,
        ))
    );
    assert_eq!(
        suru::tui::SemanticCommandId::ModelOptionReasoningCycle.as_str(),
        "model.option.reasoning.cycle"
    );

    let mut application = Application::default();
    type_terminal_text(&mut application, "/");
    let autocomplete = rendered_application_rows(&application).join("\n");
    assert!(autocomplete.contains("Configure Model Options"));
    assert!(!autocomplete.contains("Cycle Reasoning Effort"));
}

#[test]
fn reasoning_cycle_advances_provider_order_wrapping_through_the_default() {
    let mut application = Application::default();
    warm_model_catalog(
        &mut application,
        cycling_model(cycling_effort_ladder(), "medium"),
    );

    // The landing default starts at Medium, so advertised order continues High.
    let ApplicationTransition::ConfirmLandingAgentSelection(high) =
        press_reasoning_cycle(&mut application)
    else {
        panic!("the first landing selection should dispatch immediately");
    };
    assert_reasoning_summary(&application, "High");

    assert_eq!(
        press_reasoning_cycle(&mut application),
        ApplicationTransition::Continue
    );
    assert_reasoning_summary(&application, "Low");

    // The explicit default participates as an ordinary choice on the wrap.
    assert_eq!(
        press_reasoning_cycle(&mut application),
        ApplicationTransition::Continue
    );
    assert_reasoning_summary(&application, "Medium");

    let ApplicationTransition::ConfirmLandingAgentSelection(latest) = application
        .handle_event(ApplicationEvent::LandingAgentSelectionConfirmed(high))
        .expect("settle the first landing confirmation")
    else {
        panic!("settling should flush only the latest coalesced landing selection");
    };
    assert_eq!(
        latest
            .options
            .iter()
            .find(|option| option.id == ModelOptionId::new("reasoning_effort"))
            .map(|option| &option.value),
        Some(&ModelOptionValue::Select {
            choice: ModelOptionChoiceId::new("medium"),
        })
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::LandingAgentSelectionConfirmed(latest))
            .expect("settle the latest landing confirmation"),
        ApplicationTransition::Continue
    );
}

#[test]
fn reasoning_cycle_reports_unavailable_effort_without_mutation() {
    let mut single = Application::default();
    warm_model_catalog(
        &mut single,
        cycling_model(
            vec![
                cycling_choice("medium", "Medium", ModelAvailability::Available),
                cycling_choice("high", "High", ModelAvailability::Unavailable),
            ],
            "medium",
        ),
    );
    assert_eq!(
        press_reasoning_cycle(&mut single),
        ApplicationTransition::Continue
    );
    let rows = reasoning_summary(&single);
    assert!(rows.contains("Cycling GPT has no alternate Reasoning Effort choice"));
    assert!(
        !rows.contains("codex · Cycling GPT · Medium · Off"),
        "no selection is staged"
    );

    let mut without = Application::default();
    let mut model = cycling_model(Vec::new(), "medium");
    model.options.remove(0);
    warm_model_catalog(&mut without, model);
    assert_eq!(
        press_reasoning_cycle(&mut without),
        ApplicationTransition::Continue
    );
    assert!(reasoning_summary(&without).contains("Cycling GPT has no Reasoning Effort option"));
}

#[test]
fn reasoning_cycle_without_a_cached_catalog_requests_models_and_reports() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(SessionId::new(), workspace.path(), cycling_selection("low")),
        ))
        .expect("attach selected Session");

    assert!(matches!(
        press_reasoning_cycle(&mut application),
        ApplicationTransition::ListModels(_)
    ));
    assert!(reasoning_summary(&application).contains("No concrete Model is loaded yet"));
    // Nothing is pending, so Session navigation stays available.
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("navigate after the concise report"),
        ApplicationTransition::ListSessions(_)
    ));
}

#[test]
fn rapid_reasoning_cycles_coalesce_to_one_serialized_latest_selection() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(session_id, workspace.path(), cycling_selection("low")),
        ))
        .expect("attach selected Session");
    warm_model_catalog(
        &mut application,
        cycling_model(cycling_effort_ladder(), "medium"),
    );

    let ApplicationTransition::UpdateAgentSelection {
        session: first_session,
        request: first_request,
    } = press_reasoning_cycle(&mut application)
    else {
        panic!("the first cycle should dispatch one selection request");
    };
    assert_eq!(first_session.origin, suru::protocol::Outlook::Local);
    assert_eq!(first_session.session_id, session_id);
    assert_eq!(first_request.selection, cycling_selection("medium"));
    assert_reasoning_summary(&application, "Medium");

    // Later presses coalesce while the first request stays in flight.
    assert_eq!(
        press_reasoning_cycle(&mut application),
        ApplicationTransition::Continue
    );
    assert_reasoning_summary(&application, "High");
    assert_eq!(
        press_reasoning_cycle(&mut application),
        ApplicationTransition::Continue
    );
    assert_reasoning_summary(&application, "Low");

    type_terminal_text(&mut application, "must wait");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("block Prompt submission while selections settle"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("block Session navigation while selections settle"),
        ApplicationTransition::Continue
    );

    // Settling the first request flushes only the coalesced latest selection.
    let ApplicationTransition::UpdateAgentSelection {
        session: flushed_session,
        request: flushed_request,
    } = application
        .handle_event(ApplicationEvent::AgentSelectionUpdated {
            operation_id: first_request.operation_id,
            selection: cycling_selection("medium"),
        })
        .expect("settle the first selection request")
    else {
        panic!("settling should flush the coalesced latest selection");
    };
    assert_eq!(flushed_session.origin, suru::protocol::Outlook::Local);
    assert_eq!(flushed_session.session_id, session_id);
    assert_eq!(flushed_request.selection, cycling_selection("low"));
    assert_ne!(flushed_request.operation_id, first_request.operation_id);
    assert_reasoning_summary(&application, "Low");

    assert_eq!(
        application
            .handle_event(ApplicationEvent::AgentSelectionUpdated {
                operation_id: flushed_request.operation_id,
                selection: cycling_selection("low"),
            })
            .expect("settle the coalesced selection"),
        ApplicationTransition::Continue
    );
    assert_reasoning_summary(&application, "Low");
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("navigate once every selection settled"),
        ApplicationTransition::ListSessions(_)
    ));
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("submit once every selection settled"),
        ApplicationTransition::AdmitPrompt { .. }
    ));
}

#[test]
fn stale_selection_results_cannot_overwrite_newer_intent_or_roll_back() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(session_id, workspace.path(), cycling_selection("low")),
        ))
        .expect("attach selected Session");
    warm_model_catalog(
        &mut application,
        cycling_model(cycling_effort_ladder(), "medium"),
    );

    let ApplicationTransition::UpdateAgentSelection {
        request: first_request,
        ..
    } = press_reasoning_cycle(&mut application)
    else {
        panic!("the first cycle should dispatch one selection request");
    };
    assert_eq!(
        press_reasoning_cycle(&mut application),
        ApplicationTransition::Continue
    );
    assert_reasoning_summary(&application, "High");

    // Stale settlements for unknown operations change nothing.
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AgentSelectionUpdated {
                operation_id: AgentSelectionOperationId::new(),
                selection: cycling_selection("high"),
            })
            .expect("ignore a stale success"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AgentSelectionUpdateFailed {
                operation_id: AgentSelectionOperationId::new(),
                error: "stale failure".to_owned(),
            })
            .expect("ignore a stale failure"),
        ApplicationTransition::Continue
    );
    let unaffected = reasoning_summary(&application);
    assert_reasoning_summary_text(&unaffected, "High");
    assert!(!unaffected.contains("stale failure"));

    // An earlier failure must not roll back the newer queued selection.
    let ApplicationTransition::UpdateAgentSelection {
        request: retried_request,
        ..
    } = application
        .handle_event(ApplicationEvent::AgentSelectionUpdateFailed {
            operation_id: first_request.operation_id,
            error: "Effort rejected early".to_owned(),
        })
        .expect("supersede the early failure")
    else {
        panic!("the queued selection should dispatch after the failure");
    };
    assert_eq!(retried_request.selection, cycling_selection("high"));
    let superseded = reasoning_summary(&application);
    assert_reasoning_summary_text(&superseded, "High");
    assert!(!superseded.contains("Effort rejected early"));

    // Failing the latest remaining selection reveals the authoritative state.
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AgentSelectionUpdateFailed {
                operation_id: retried_request.operation_id,
                error: "Effort rejected".to_owned(),
            })
            .expect("fail the latest remaining selection"),
        ApplicationTransition::Continue
    );
    assert!(reasoning_summary(&application).contains("Effort rejected"));
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("navigate after the rollback"),
        ApplicationTransition::ListSessions(_)
    ));
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::ModelOptions,
            )))
            .expect("reopen rolled-back options"),
        ApplicationTransition::ListModels(_)
    ));
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Reasoning · Low")
    );
}

#[test]
fn authoritative_updates_slide_beneath_the_optimistic_overlay_until_settled() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(session_id, workspace.path(), cycling_selection("low")),
        ))
        .expect("attach selected Session");
    warm_model_catalog(
        &mut application,
        cycling_model(cycling_effort_ladder(), "medium"),
    );

    let ApplicationTransition::UpdateAgentSelection {
        request: first_request,
        ..
    } = press_reasoning_cycle(&mut application)
    else {
        panic!("the first cycle should dispatch one selection request");
    };
    assert_eq!(
        press_reasoning_cycle(&mut application),
        ApplicationTransition::Continue
    );

    // Another client's authoritative change lands beneath the local overlay.
    let mut remote = cycling_selection("low");
    remote.options[1].value = ModelOptionValue::Toggle { enabled: true };
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(2),
                changes: vec![SessionChange::AgentSelectionChanged { selection: remote }],
            },
        )))
        .expect("apply another client's authoritative selection");
    let overlaid = reasoning_summary(&application);
    assert_reasoning_summary_text(&overlaid, "High");
    assert!(!overlaid.contains("codex · Cycling GPT · High · On"));

    let ApplicationTransition::UpdateAgentSelection {
        request: flushed_request,
        ..
    } = application
        .handle_event(ApplicationEvent::AgentSelectionUpdated {
            operation_id: first_request.operation_id,
            selection: cycling_selection("medium"),
        })
        .expect("settle the first selection request")
    else {
        panic!("settling should flush the coalesced latest selection");
    };
    assert_eq!(flushed_request.selection, cycling_selection("high"));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AgentSelectionUpdated {
                operation_id: flushed_request.operation_id,
                selection: cycling_selection("high"),
            })
            .expect("settle the coalesced selection"),
        ApplicationTransition::Continue
    );
    assert_reasoning_summary(&application, "High");

    // Once local work settles, server acceptance order is authoritative.
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(3),
                changes: vec![SessionChange::AgentSelectionChanged {
                    selection: cycling_selection("medium"),
                }],
            },
        )))
        .expect("apply the first accepted selection");
    assert_reasoning_summary(&application, "Medium");
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(4),
                changes: vec![SessionChange::AgentSelectionChanged {
                    selection: cycling_selection("high"),
                }],
            },
        )))
        .expect("apply the last accepted selection");
    assert_reasoning_summary(&application, "High");
}
