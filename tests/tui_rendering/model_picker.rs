//! The model picker: listing, search, focus, and selection.

use crate::support::{
    model_descriptor, rendered_application_rows, rendered_application_rows_at, rendered_row,
    selected_session_snapshot, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        AgentSelection, ModelAvailability, ModelCatalog, ModelId, ModelOptionChoice,
        ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
        ModelOptionRole, ModelOptionValue, ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
        ProviderUnavailability, SessionChange, SessionId, SessionRevision, SessionUpdate,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId},
};

#[test]
fn model_picker_hands_off_to_ordered_options_and_applies_complete_landing_selection() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    let mut model = model_descriptor(
        "codex",
        "gpt-configurable",
        "Configurable GPT",
        true,
        ModelAvailability::Available,
    );
    model.options = vec![
        ModelOptionDescriptor {
            id: ModelOptionId::new("reasoning_effort"),
            label: "Reasoning".to_owned(),
            description: Some("Depth used to solve the Prompt".to_owned()),
            role: ModelOptionRole::ReasoningEffort,
            kind: ModelOptionKind::Select {
                choices: vec![
                    ModelOptionChoice {
                        id: ModelOptionChoiceId::new("low"),
                        label: "Low".to_owned(),
                        description: Some("Respond quickly".to_owned()),
                        availability: ModelAvailability::Available,
                    },
                    ModelOptionChoice {
                        id: ModelOptionChoiceId::new("high"),
                        label: "High".to_owned(),
                        description: Some("Think deeply".to_owned()),
                        availability: ModelAvailability::Available,
                    },
                ],
                default: ModelOptionChoiceId::new("low"),
            },
        },
        ModelOptionDescriptor {
            id: ModelOptionId::new("fast"),
            label: "Fast".to_owned(),
            description: Some("Prefer low latency".to_owned()),
            role: ModelOptionRole::Speed,
            kind: ModelOptionKind::Toggle { default: false },
        },
    ];
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "Codex".to_owned(),
                    models: vec![model],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("load configurable Model");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select configurable Model"),
        ApplicationTransition::Continue
    );
    let options = rendered_application_rows(&application);
    assert!(options.join("\n").contains("Model Options"));
    assert!(options.join("\n").contains("Configurable GPT"));
    assert!(options.join("\n").contains("Provider Codex"));
    assert!(rendered_row(&options, "Reasoning") < rendered_row(&options, "Fast"));
    assert!(options.join("\n").contains("Reasoning · Low"));
    assert!(options.join("\n").contains("Fast · Off"));
    assert!(
        options
            .join("\n")
            .contains("Depth used to solve the Prompt")
    );

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("open Reasoning choices");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Think deeply")
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus High reasoning");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("stage High reasoning");

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus Fast option");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("open Fast choices");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )))
        .expect("focus On");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("stage Fast On");

    let staged = rendered_application_rows(&application).join("\n");
    assert!(staged.contains("Reasoning · High"));
    assert!(staged.contains("Fast · On"));
    let ApplicationTransition::ConfirmLandingAgentSelection(confirmed) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("apply complete options")
    else {
        panic!("landing options should confirm the complete Agent Selection");
    };
    assert_eq!(confirmed.model, ModelId::new("gpt-configurable"));
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("Model Options")
    );

    type_terminal_text(&mut application, "Create with options");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit landing selection")
    else {
        panic!("landing Prompt should create a Session");
    };
    assert_eq!(
        request.agent_selection,
        Some(AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-configurable"),
            options: vec![
                suru::protocol::ModelOptionSelection {
                    id: ModelOptionId::new("reasoning_effort"),
                    value: ModelOptionValue::Select {
                        choice: ModelOptionChoiceId::new("high"),
                    },
                },
                suru::protocol::ModelOptionSelection {
                    id: ModelOptionId::new("fast"),
                    value: ModelOptionValue::Toggle { enabled: true },
                },
            ],
        })
    );
}

#[test]
fn landing_model_picker_groups_sorts_focuses_and_searches_models() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Loading Models")
    );

    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![
                    ProviderModelCatalog {
                        provider: ProviderId::new("zeta"),
                        display_name: "zeta".to_owned(),
                        models: vec![model_descriptor(
                            "zeta",
                            "z-native",
                            "Zebra",
                            false,
                            ModelAvailability::Available,
                        )],
                        status: ProviderCatalogStatus::Fresh,
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("alpha"),
                        display_name: "alpha".to_owned(),
                        models: vec![
                            model_descriptor(
                                "alpha",
                                "alpha-pro-2026",
                                "Alpha Pro",
                                false,
                                ModelAvailability::Available,
                            ),
                            model_descriptor(
                                "alpha",
                                "alpha-default",
                                "Default",
                                true,
                                ModelAvailability::Available,
                            ),
                            model_descriptor(
                                "alpha",
                                "alpha-native-identifier-that-must-remain-visible",
                                "An exceptionally long Model display name that would consume the entire picker row",
                                false,
                                ModelAvailability::Unavailable,
                            ),
                        ],
                        status: ProviderCatalogStatus::Refreshing,
                    },
                ],
            },
        })
        .expect("load cached Model catalog");

    let rows = rendered_application_rows(&application);
    assert!(rendered_row(&rows, "Provider alpha") < rendered_row(&rows, "Provider zeta"));
    assert!(rendered_row(&rows, "Alpha Pro") < rendered_row(&rows, "Default"));
    let default = rows
        .iter()
        .find(|row| row.contains("alpha-default"))
        .expect("render Provider default Model");
    assert!(default.contains("default"));
    assert!(default.contains('›'));
    assert!(
        rows.iter()
            .find(|row| row.contains("Alpha Pro"))
            .expect("render display name")
            .contains("alpha-pro-2026")
    );
    let long = rows
        .iter()
        .find(|row| row.contains("An exceptionally"))
        .expect("render long Model row");
    assert!(long.contains("alpha-native"));
    assert!(long.contains("unavailable"));

    type_terminal_text(&mut application, "z-native");
    let searched = rendered_application_rows(&application).join("\n");
    assert!(searched.contains("Zebra"));
    assert!(!searched.contains("Alpha Pro"));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close cached picker");
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::ModelList,
            )))
            .expect("reopen cached picker and request background refresh"),
        ApplicationTransition::ListModels(_)
    ));
    let reopened = rendered_application_rows(&application).join("\n");
    assert!(reopened.contains("Alpha Pro"));
    assert!(!reopened.contains("Loading Models"));
}

#[test]
fn open_model_picker_merges_refreshes_stably_and_isolates_provider_failures() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request: request.clone(),
            catalog: ModelCatalog {
                providers: vec![
                    ProviderModelCatalog {
                        provider: ProviderId::new("alpha"),
                        display_name: "alpha".to_owned(),
                        models: vec![
                            model_descriptor(
                                "alpha",
                                "alpha-pro",
                                "Alpha Pro",
                                false,
                                ModelAvailability::Available,
                            ),
                            model_descriptor(
                                "alpha",
                                "default",
                                "Default",
                                true,
                                ModelAvailability::Available,
                            ),
                        ],
                        status: ProviderCatalogStatus::Refreshing,
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("broken"),
                        display_name: "broken".to_owned(),
                        models: Vec::new(),
                        status: ProviderCatalogStatus::Failed {
                            message: "credentials expired".to_owned(),
                        },
                    },
                ],
            },
        })
        .expect("show cached Models");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("focus Alpha Pro");

    application
        .handle_event(ApplicationEvent::ModelsRefreshed {
            request,
            catalog: ModelCatalog {
                providers: vec![
                    ProviderModelCatalog {
                        provider: ProviderId::new("alpha"),
                        display_name: "alpha".to_owned(),
                        models: vec![
                            model_descriptor(
                                "alpha",
                                "default",
                                "Default renamed",
                                true,
                                ModelAvailability::Available,
                            ),
                            model_descriptor(
                                "alpha",
                                "new",
                                "Aardvark New",
                                false,
                                ModelAvailability::Available,
                            ),
                        ],
                        status: ProviderCatalogStatus::Fresh,
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("broken"),
                        display_name: "broken".to_owned(),
                        models: Vec::new(),
                        status: ProviderCatalogStatus::Failed {
                            message: "credentials expired".to_owned(),
                        },
                    },
                ],
            },
        })
        .expect("merge refreshed Models");

    let rows = rendered_application_rows(&application);
    let removed = rows
        .iter()
        .find(|row| row.contains("Alpha Pro"))
        .expect("keep removed Model in place");
    assert!(removed.contains("unavailable"));
    assert!(removed.contains('›'));
    assert!(rendered_row(&rows, "Alpha Pro") < rendered_row(&rows, "Default renamed"));
    assert!(rendered_row(&rows, "Default renamed") < rendered_row(&rows, "Aardvark New"));
    assert!(
        rows.join("\n")
            .contains("Retry broken: credentials expired")
    );

    for _ in 0..3 {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Down,
                KeyModifiers::NONE,
            )))
            .expect("navigate to Provider retry");
    }
    assert!(matches!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("retry failed Provider"),
        ApplicationTransition::ListModels(_)
    ));
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close retrying picker");
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::ModelList,
            )))
            .expect("reopen from the latest normalized cache"),
        ApplicationTransition::ListModels(_)
    ));
    let reopened = rendered_application_rows(&application);
    assert!(!reopened.join("\n").contains("Alpha Pro"));
    assert!(rendered_row(&reopened, "Aardvark New") < rendered_row(&reopened, "Default renamed"));
}

#[test]
fn model_picker_refresh_failure_preserves_the_visible_selection() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request: request.clone(),
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("alpha"),
                    display_name: "alpha".to_owned(),
                    models: vec![
                        model_descriptor(
                            "alpha",
                            "alpha-pro",
                            "Alpha Pro",
                            false,
                            ModelAvailability::Available,
                        ),
                        model_descriptor(
                            "alpha",
                            "default",
                            "Default",
                            true,
                            ModelAvailability::Available,
                        ),
                    ],
                    status: ProviderCatalogStatus::Refreshing,
                }],
            },
        })
        .expect("show cached Models");
    application
        .handle_event(ApplicationEvent::ModelListingFailed {
            request,
            error: "refresh timed out".to_owned(),
        })
        .expect("show stale cached Models");

    let rows = rendered_application_rows(&application);
    assert!(
        rows.iter()
            .find(|row| row.contains("Default"))
            .expect("keep the selected cached Model")
            .contains('›')
    );
    assert!(rows.join("\n").contains("refresh timed out"));
}

#[test]
fn session_model_selection_is_provider_scoped_optimistic_and_rolls_back_locally() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let authoritative = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("old"),
        options: Vec::new(),
    };
    let snapshot = selected_session_snapshot(session_id, workspace.path(), authoritative.clone());
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach selected Session");
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Session Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    let mut new_model = model_descriptor(
        "codex",
        "new",
        "New Model",
        false,
        ModelAvailability::Available,
    );
    new_model.options = vec![
        ModelOptionDescriptor {
            id: ModelOptionId::new("reasoning_effort"),
            label: "Reasoning".to_owned(),
            description: None,
            role: ModelOptionRole::ReasoningEffort,
            kind: ModelOptionKind::Select {
                choices: vec![
                    ModelOptionChoice {
                        id: ModelOptionChoiceId::new("low"),
                        label: "Low".to_owned(),
                        description: None,
                        availability: ModelAvailability::Available,
                    },
                    ModelOptionChoice {
                        id: ModelOptionChoiceId::new("high"),
                        label: "High".to_owned(),
                        description: None,
                        availability: ModelAvailability::Available,
                    },
                ],
                default: ModelOptionChoiceId::new("high"),
            },
        },
        ModelOptionDescriptor {
            id: ModelOptionId::new("fast"),
            label: "Fast".to_owned(),
            description: None,
            role: ModelOptionRole::Speed,
            kind: ModelOptionKind::Toggle { default: true },
        },
    ];
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![
                    ProviderModelCatalog {
                        provider: ProviderId::new("other"),
                        display_name: "other".to_owned(),
                        models: vec![model_descriptor(
                            "other",
                            "foreign",
                            "Foreign Model",
                            true,
                            ModelAvailability::Available,
                        )],
                        status: ProviderCatalogStatus::Fresh,
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("codex"),
                        display_name: "Codex".to_owned(),
                        models: vec![
                            model_descriptor(
                                "codex",
                                "old",
                                "Old Model",
                                true,
                                ModelAvailability::Available,
                            ),
                            new_model,
                        ],
                        status: ProviderCatalogStatus::Fresh,
                    },
                ],
            },
        })
        .expect("load Session-scoped catalog");
    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Session Provider Codex"));
    assert!(picker.contains("use /new to change Provider"));
    assert!(!picker.contains("Foreign Model"));
    assert!(
        picker
            .lines()
            .find(|row| row.contains("Old Model"))
            .expect("render current Model")
            .contains("current")
    );
    let tiny_picker = rendered_application_rows_at(&application, 28, 5).join("\n");
    assert!(tiny_picker.contains("Old Model"));
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("focus New Model");
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select configurable New Model"),
        ApplicationTransition::Continue
    );
    let tiny_options = rendered_application_rows_at(&application, 28, 5).join("\n");
    assert!(tiny_options.contains("Reasoning"));
    let ApplicationTransition::UpdateAgentSelection {
        session_id: updated_session,
        request,
    } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("apply New Model defaults")
    else {
        panic!("applying a Session Model's options should update Agent Selection");
    };
    assert_eq!(updated_session, session_id);
    assert_eq!(request.selection.model, ModelId::new("new"));
    assert_eq!(
        request.selection.options,
        vec![
            suru::protocol::ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high"),
                },
            },
            suru::protocol::ModelOptionSelection {
                id: ModelOptionId::new("fast"),
                value: ModelOptionValue::Toggle { enabled: true },
            },
        ]
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Model New Model")
    );
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::ModelList,
            )))
            .expect("reopen picker around optimistic Model"),
        ApplicationTransition::ListModels(_)
    ));
    let optimistic_picker = rendered_application_rows(&application);
    let optimistic = optimistic_picker
        .iter()
        .find(|row| row.contains("New Model"))
        .expect("render optimistic Model row");
    assert!(optimistic.contains("current"));
    assert!(optimistic.contains('›'));
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("prevent rapid reselection while the first operation is pending"),
        ApplicationTransition::Continue
    );
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close optimistic Model picker");

    type_terminal_text(&mut application, "must wait");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("block Prompt during optimistic selection"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionList,
            )))
            .expect("block navigation during optimistic selection"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionNew,
            )))
            .expect("block /new during optimistic selection"),
        ApplicationTransition::Continue
    );

    let mut other_client = Application::new(workspace.path());
    other_client
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach another client");
    type_terminal_text(&mut other_client, "other client can submit");
    assert!(matches!(
        other_client
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("submit from unaffected client"),
        ApplicationTransition::AdmitPrompt { .. }
    ));

    application
        .handle_event(ApplicationEvent::AgentSelectionUpdateFailed {
            operation_id: request.operation_id,
            error: "Model rejected".to_owned(),
        })
        .expect("reject optimistic selection");
    let rolled_back = rendered_application_rows(&application).join("\n");
    assert!(rolled_back.contains("Model Old Model"));
    assert!(rolled_back.contains("Model rejected"));
    assert!(matches!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("unblock Prompt after selection rejection"),
        ApplicationTransition::AdmitPrompt { .. }
    ));
}

#[test]
fn landing_model_selection_and_new_session_inherit_complete_agent_selection() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = Application::new(workspace.path());
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open landing Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    let mut model = model_descriptor(
        "codex",
        "gpt-native",
        "GPT Friendly",
        true,
        ModelAvailability::Available,
    );
    model.options.push(ModelOptionDescriptor {
        id: ModelOptionId::new("reasoning_effort"),
        label: "Reasoning".to_owned(),
        description: None,
        role: ModelOptionRole::ReasoningEffort,
        kind: ModelOptionKind::Select {
            choices: vec![ModelOptionChoice {
                id: ModelOptionChoiceId::new("high"),
                label: "High".to_owned(),
                description: None,
                availability: ModelAvailability::Available,
            }],
            default: ModelOptionChoiceId::new("high"),
        },
    });
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "codex".to_owned(),
                    models: vec![model],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("load landing Models");
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select landing Model"),
        ApplicationTransition::Continue
    );
    let ApplicationTransition::ConfirmLandingAgentSelection(confirmed) = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )))
        .expect("apply landing Model options")
    else {
        panic!("landing options should confirm the complete Agent Selection");
    };
    assert_eq!(confirmed.model, ModelId::new("gpt-native"));
    assert_eq!(confirmed.options.len(), 1);
    let wide = rendered_application_rows_at(&application, 100, 16).join("\n");
    assert!(wide.contains("Model GPT Friendly"));
    assert!(wide.contains("Reasoning High"));
    let compact = rendered_application_rows_at(&application, 43, 10).join("\n");
    assert!(compact.contains("Model GPT Friendly"));
    assert!(!compact.contains("Reasoning High"));

    type_terminal_text(&mut application, "Create with selection");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit selected landing Model")
    else {
        panic!("landing Prompt should create a Session");
    };
    let selected = request
        .agent_selection
        .expect("landing selection is sent during Session creation");
    assert_eq!(selected.model, ModelId::new("gpt-native"));
    assert_eq!(selected.options.len(), 1);

    let inherited = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("inherited"),
        options: vec![suru::protocol::ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new("medium"),
            },
        }],
    };
    let mut attached = Application::new(workspace.path());
    attached
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(SessionId::new(), workspace.path(), inherited.clone()),
        ))
        .expect("attach Session before /new");
    assert_eq!(
        attached
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                suru::tui::SemanticCommandId::SessionNew,
            )))
            .expect("start inherited /new flow"),
        ApplicationTransition::DetachSession
    );
    type_terminal_text(&mut attached, "Inherited work");
    let ApplicationTransition::CreateSession(request) = attached
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("create inherited Session")
    else {
        panic!("/new landing Prompt should create a Session");
    };
    assert_eq!(request.agent_selection, Some(inherited));
}

#[test]
fn open_model_picker_refocuses_on_an_authoritative_multi_client_update() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let session_id = SessionId::new();
    let old = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("old"),
        options: Vec::new(),
    };
    let new = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("new"),
        options: Vec::new(),
    };
    let mut observer = Application::new(workspace.path());
    observer
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(session_id, workspace.path(), old),
        ))
        .expect("attach observing client");
    let ApplicationTransition::ListModels(request) = observer
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open observer Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    let catalog = ModelCatalog {
        providers: vec![ProviderModelCatalog {
            provider: ProviderId::new("codex"),
            display_name: "codex".to_owned(),
            models: vec![
                model_descriptor(
                    "codex",
                    "old",
                    "Old Model",
                    true,
                    ModelAvailability::Available,
                ),
                model_descriptor(
                    "codex",
                    "new",
                    "New Model",
                    false,
                    ModelAvailability::Available,
                ),
            ],
            status: ProviderCatalogStatus::Fresh,
        }],
    };
    observer
        .handle_event(ApplicationEvent::ModelsListed {
            request: request.clone(),
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "codex".to_owned(),
                    models: vec![model_descriptor(
                        "codex",
                        "new",
                        "New Model",
                        true,
                        ModelAvailability::Available,
                    )],
                    status: ProviderCatalogStatus::Refreshing,
                }],
            },
        })
        .expect("load cache without the authoritative Model");
    assert!(
        rendered_application_rows(&observer)
            .iter()
            .find(|row| row.contains("New Model"))
            .expect("focus fallback Model")
            .contains('›')
    );
    observer
        .handle_event(ApplicationEvent::ModelsListed {
            request: request.clone(),
            catalog: catalog.clone(),
        })
        .expect("merge a newly available authoritative Model");
    assert!(
        rendered_application_rows(&observer)
            .iter()
            .find(|row| row.contains("Old Model"))
            .expect("render newly available authoritative Model")
            .contains('›')
    );
    observer
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )))
        .expect("move away from current Model before refresh");
    observer
        .handle_event(ApplicationEvent::ModelsRefreshed { request, catalog })
        .expect("merge refresh without moving the cursor");
    let refreshed = rendered_application_rows(&observer);
    assert!(
        refreshed
            .iter()
            .find(|row| row.contains("New Model"))
            .expect("render manually focused Model")
            .contains('›')
    );
    assert!(
        refreshed
            .iter()
            .find(|row| row.contains("Old Model"))
            .expect("render authoritative Model")
            .contains("current")
    );

    observer
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(2),
                changes: vec![SessionChange::AgentSelectionChanged { selection: new }],
            },
        )))
        .expect("apply another client's authoritative selection");
    let rows = rendered_application_rows(&observer);
    let new_row = rows
        .iter()
        .find(|row| row.contains("New Model"))
        .expect("render remotely selected Model");
    assert!(new_row.contains("current"));
    assert!(new_row.contains('›'));
    assert!(
        !rows
            .iter()
            .find(|row| row.contains("Old Model"))
            .expect("keep old Model available")
            .contains('›')
    );
}

#[test]
fn model_picker_shows_an_unavailable_provider_with_its_reason_and_refuses_selection() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request: request.clone(),
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "codex".to_owned(),
                    models: vec![model_descriptor(
                        "codex",
                        "gpt-fixture",
                        "GPT Fixture",
                        true,
                        ModelAvailability::Available,
                    )],
                    status: ProviderCatalogStatus::Unavailable {
                        reason: ProviderUnavailability::NotInstalled,
                        message: "could not launch Codex app-server `codex`".to_owned(),
                    },
                }],
            },
        })
        .expect("load an unavailable Provider");

    let rows = rendered_application_rows(&application);
    let screen = rows.join("\n");
    assert!(
        screen.contains("Retry codex: not installed"),
        "the picker states the typed reason, got {screen}"
    );
    assert!(
        screen.contains("could not launch Codex app-server"),
        "the picker keeps the Provider's own account of the condition, got {screen}"
    );
    assert!(
        rows.iter()
            .find(|row| row.contains("GPT Fixture"))
            .expect("the unavailable Provider keeps its place with its Models")
            .contains("unavailable"),
        "no Model of an unavailable Provider may be selected, got {screen}"
    );

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("attempt to select an unavailable Model"),
        ApplicationTransition::Continue
    );
    let refused = rendered_application_rows(&application).join("\n");
    assert!(
        !refused.contains("Model Options"),
        "selecting an unavailable Model does nothing, got {refused}"
    );
    assert!(
        refused.contains("Retry codex: not installed"),
        "the picker stays open on the unavailable Provider, got {refused}"
    );

    // The user installs the CLI; the picker's own refresh clears the condition
    // without a restart, and the Model becomes selectable.
    application
        .handle_event(ApplicationEvent::ModelsRefreshed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "codex".to_owned(),
                    models: vec![model_descriptor(
                        "codex",
                        "gpt-fixture",
                        "GPT Fixture",
                        true,
                        ModelAvailability::Available,
                    )],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("merge the repaired Provider");
    let repaired = rendered_application_rows(&application);
    assert!(
        !repaired.join("\n").contains("not installed"),
        "a fixed Provider drops its reason, got {}",
        repaired.join("\n")
    );
    assert!(
        !repaired
            .iter()
            .find(|row| row.contains("GPT Fixture"))
            .expect("the repaired Provider keeps its Models")
            .contains("unavailable")
    );
    assert!(
        matches!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))
                .expect("select the repaired Model"),
            ApplicationTransition::ConfirmLandingAgentSelection(selection)
                if selection.model == ModelId::new("gpt-fixture")
        ),
        "the Model a fixed Provider serves is selectable again"
    );
}

/// A disabled Provider is the opposite of an unavailable one here: it draws no
/// row at all rather than keeping its place with a retry row, because Suru
/// never consulted it and decluttering the list is half of what the Setting is
/// for.
#[test]
fn model_picker_draws_no_row_at_all_for_a_provider_the_user_turned_off() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request: request.clone(),
            catalog: ModelCatalog {
                providers: vec![
                    ProviderModelCatalog {
                        provider: ProviderId::new("codex"),
                        display_name: "codex".to_owned(),
                        models: Vec::new(),
                        status: ProviderCatalogStatus::Disabled,
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("copilot"),
                        display_name: "copilot".to_owned(),
                        models: Vec::new(),
                        status: ProviderCatalogStatus::Unavailable {
                            reason: ProviderUnavailability::NotInstalled,
                            message: "could not launch the Copilot CLI".to_owned(),
                        },
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("other"),
                        display_name: "other".to_owned(),
                        models: vec![model_descriptor(
                            "other",
                            "other-default",
                            "Other Default",
                            true,
                            ModelAvailability::Available,
                        )],
                        status: ProviderCatalogStatus::Fresh,
                    },
                ],
            },
        })
        .expect("load a catalog holding a disabled Provider");

    let screen = rendered_application_rows(&application).join("\n");
    assert!(
        !screen.contains("codex"),
        "a disabled Provider leaves the list outright, got {screen}"
    );
    assert!(
        screen.contains("Retry copilot: not installed"),
        "an unavailable Provider still keeps its place with its retry row, got {screen}"
    );
    assert!(
        screen.contains("Other Default"),
        "the Providers the user kept are unaffected, got {screen}"
    );

    // The user turns it back on; its Models arrive with the next listing and
    // become selectable without anything else happening.
    application
        .handle_event(ApplicationEvent::ModelsRefreshed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "codex".to_owned(),
                    models: vec![model_descriptor(
                        "codex",
                        "gpt-fixture",
                        "GPT Fixture",
                        true,
                        ModelAvailability::Available,
                    )],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("merge the re-enabled Provider");
    let rows = rendered_application_rows(&application);
    assert!(
        rows.join("\n").contains("Provider codex"),
        "the Provider returns to the list the moment it is enabled, got {}",
        rows.join("\n")
    );
    assert!(
        !rows
            .iter()
            .find(|row| row.contains("GPT Fixture"))
            .expect("the re-enabled Provider brings its Models")
            .contains("unavailable"),
        "its Models are selectable again"
    );
}

#[test]
fn model_picker_keeps_an_unavailable_provider_with_no_models_in_the_list() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![
                    ProviderModelCatalog {
                        provider: ProviderId::new("codex"),
                        display_name: "codex".to_owned(),
                        models: Vec::new(),
                        status: ProviderCatalogStatus::Unavailable {
                            reason: ProviderUnavailability::NotInstalled,
                            message: "could not launch Codex app-server `codex`".to_owned(),
                        },
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("other"),
                        display_name: "other".to_owned(),
                        models: vec![model_descriptor(
                            "other",
                            "other-default",
                            "Other Default",
                            true,
                            ModelAvailability::Available,
                        )],
                        status: ProviderCatalogStatus::Fresh,
                    },
                ],
            },
        })
        .expect("load a Provider that served nothing");

    let rows = rendered_application_rows(&application);
    let screen = rows.join("\n");
    assert!(
        screen.contains("Provider codex"),
        "a Provider with no Models still keeps its place, got {screen}"
    );
    assert!(
        screen.contains("Retry codex: not installed"),
        "its reason stands in for the Models it has none of, got {screen}"
    );
    assert!(
        rendered_row(&rows, "Provider codex") < rendered_row(&rows, "Retry codex"),
        "the reason sits under its own Provider, got {screen}"
    );
    assert!(
        rendered_row(&rows, "Retry codex") < rendered_row(&rows, "Provider other"),
        "the available Provider follows rather than absorbing the reason, got {screen}"
    );
    assert!(
        !screen.contains("No Models found"),
        "the list is not empty, got {screen}"
    );
}

/// Issue #136: rows read as products, not wire identifiers. The catalog
/// carries each Provider's runtime-declared display name, and the picker
/// prints it on the Provider heading and the retry row alike.
#[test]
fn model_picker_shows_provider_display_names_from_the_catalog() {
    let mut application = Application::default();
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            suru::tui::SemanticCommandId::ModelList,
        )))
        .expect("open Model picker")
    else {
        panic!("Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![
                    ProviderModelCatalog {
                        provider: ProviderId::new("codex"),
                        display_name: "Codex".to_owned(),
                        models: vec![model_descriptor(
                            "codex",
                            "gpt",
                            "GPT",
                            true,
                            ModelAvailability::Available,
                        )],
                        status: ProviderCatalogStatus::Fresh,
                    },
                    ProviderModelCatalog {
                        provider: ProviderId::new("copilot"),
                        display_name: "Copilot".to_owned(),
                        models: Vec::new(),
                        status: ProviderCatalogStatus::Unavailable {
                            reason: ProviderUnavailability::NotSignedIn,
                            message: "run `copilot` to sign in".to_owned(),
                        },
                    },
                ],
            },
        })
        .expect("load catalog carrying display names");

    let screen = rendered_application_rows(&application).join("\n");
    assert!(
        screen.contains("Provider Codex"),
        "the heading reads the display name, got {screen}"
    );
    assert!(
        !screen.contains("Provider codex"),
        "the wire identifier never reaches the screen, got {screen}"
    );
    assert!(
        screen.contains("Retry Copilot: not signed in"),
        "the retry row reads the display name, got {screen}"
    );
    assert!(
        !screen.contains("Retry copilot"),
        "the retry row never falls back to the wire identifier, got {screen}"
    );
}
