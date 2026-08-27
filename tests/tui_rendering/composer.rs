//! Composer input: cursor placement, growth, editing bindings, and history.

use crate::support::{
    WorkspaceDir, buffer_rows, enter_session, failed_session_snapshot, rendered_application_buffer,
    rendered_application_cursor_at, rendered_application_rows, rendered_application_rows_at,
    text_position, type_terminal_text, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::Color;
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        AgentSelection, ModelId, PromptId, ProviderId, SessionId, SkillCatalog,
        SkillCatalogCapabilities, SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor,
        SkillId, SkillPromptDelivery, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, command_for_terminal_event,
    },
};
use uuid::Uuid;

use crate::support::ready_health;

#[test]
fn composer_cursor_tracks_empty_unicode_and_multiline_input() {
    let mut application = Application::default();
    let empty = rendered_application_buffer(&application, 80, 15);
    let placeholder = text_position(&empty, "Type a Prompt and press Enter");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(placeholder.0, placeholder.1)
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "a🙂β\nsecond".to_owned(),
        )))
        .expect("type multiline Unicode Prompt");
    let multiline = rendered_application_buffer(&application, 80, 15);
    let second = text_position(&multiline, "second");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(second.0 + 6, second.1)
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
        .expect("move cursor within the second line");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(second.0 + 5, second.1)
    );

    for _ in 0..7 {
        application
            .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
            .expect("move cursor onto the Unicode first line");
    }
    let first = text_position(&multiline, "a🙂");
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 15),
        Position::new(first.0 + 3, first.1),
        "the emoji occupies two terminal cells"
    );
}

#[test]
fn skill_completion_replaces_only_the_query_binds_it_and_keeps_the_prompt_open() {
    let workspace = workspace_dir();
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-fixture"),
        options: Vec::new(),
    };
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(Uuid::new_v4(), 42).with_landing_agent_selection(Some(selection)),
        )))
        .expect("connect application");
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };
    let skill = SkillDescriptor {
        id: SkillId::new("opaque-review-id"),
        name: "review".to_owned(),
        description: "Review the current change".to_owned(),
        scope: Some("Workspace".to_owned()),
    };
    application
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider.clone(),
                workspace: request.workspace.clone(),
                skills: vec![skill.clone()],
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations: None,
                    supported_deliveries: vec![SkillPromptDelivery::Initial],
                },
                status: SkillCatalogStatus::Fresh { warning: None },
            },
        })
        .expect("load Skill Catalog");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Please $rev".to_owned(),
        )))
        .expect("type a Skill query");
    let completion = rendered_application_rows(&application).join("\n");
    assert!(completion.contains(" Skills "));
    assert!(completion.contains("$review"));
    assert!(!completion.contains(" Commands "));

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(
                CommandId::ConfirmSelectedCompletion,
            ))
            .expect("confirm Skill completion"),
        ApplicationTransition::Continue,
        "Skill confirmation edits without submitting"
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "improve this".to_owned(),
        )))
        .expect("finish the Prompt");
    let ApplicationTransition::CreateSession(created) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit Prompt")
    else {
        panic!("Skill Prompt should create a Session");
    };
    assert_eq!(created.prompt.text, "Please $review improve this");
    assert_eq!(created.prompt.skill_invocations.len(), 1);
    assert_eq!(created.prompt.skill_invocations[0].skill_id, skill.id);
    assert_eq!(created.prompt.skill_invocations[0].name, "review");
    assert_eq!(created.prompt.skill_invocations[0].marker.start, 7);
    assert_eq!(created.prompt.skill_invocations[0].marker.end, 14);
}

#[test]
fn exact_typed_and_pasted_skill_markers_bind_without_completion_and_keep_punctuation() {
    let (_workspace, mut application) = application_with_skills(
        vec![SkillDescriptor {
            id: SkillId::new("opaque-review-id"),
            name: "Review".to_owned(),
            description: "Review the current change".to_owned(),
            scope: Some("Workspace".to_owned()),
        }],
        None,
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Use $review,".to_owned(),
        )))
        .expect("type an exact marker");
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            " then $REVIEW.\"".to_owned(),
        )))
        .expect("paste another exact marker");
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(" Skills "),
        "paste must not open Skill completion"
    );

    let ApplicationTransition::CreateSession(created) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit manually bound Skills")
    else {
        panic!("exact markers should produce a bound Prompt");
    };
    assert_eq!(created.prompt.text, "Use $review, then $REVIEW.\"");
    assert_eq!(created.prompt.skill_invocations.len(), 2);
    assert_eq!(created.prompt.skill_invocations[0].name, "Review");
    assert_eq!(created.prompt.skill_invocations[0].marker.start, 4);
    assert_eq!(created.prompt.skill_invocations[0].marker.end, 11);
    assert_eq!(created.prompt.skill_invocations[1].name, "Review");
    assert_eq!(created.prompt.skill_invocations[1].marker.start, 18);
    assert_eq!(created.prompt.skill_invocations[1].marker.end, 25);
}

#[test]
fn continued_typing_recomputes_manual_markers_before_final_resolution() {
    let review = SkillDescriptor {
        id: SkillId::new("review-id"),
        name: "review".to_owned(),
        description: "Review guidance".to_owned(),
        scope: None,
    };
    let lint = SkillDescriptor {
        id: SkillId::new("lint-id"),
        name: "lint".to_owned(),
        description: "General lint guidance".to_owned(),
        scope: None,
    };
    let lint_rust = SkillDescriptor {
        id: SkillId::new("lint-rust-id"),
        name: "lint.rs".to_owned(),
        description: "Rust lint guidance".to_owned(),
        scope: None,
    };

    let (_workspace, mut unknown) = application_with_skills(vec![review], None);
    type_terminal_text(&mut unknown, "$reviewer");
    let ApplicationTransition::CreateSession(created) = unknown
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit a longer unknown token")
    else {
        panic!("longer unknown token remains an ordinary Prompt");
    };
    assert!(created.prompt.skill_invocations.is_empty());

    let (_workspace, mut longest) = application_with_skills(vec![lint, lint_rust.clone()], None);
    type_terminal_text(&mut longest, "$lint.rs,");
    let ApplicationTransition::CreateSession(created) = longest
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the longest marker")
    else {
        panic!("longest marker creates a Skill-bearing Prompt");
    };
    assert_eq!(created.prompt.skill_invocations.len(), 1);
    assert_eq!(created.prompt.skill_invocations[0].skill_id, lint_rust.id);
    assert_eq!(created.prompt.skill_invocations[0].marker.start, 0);
    assert_eq!(created.prompt.skill_invocations[0].marker.end, 8);

    let unicode = SkillDescriptor {
        id: SkillId::new("unicode-id"),
        name: "Über".to_owned(),
        description: "Unicode guidance".to_owned(),
        scope: None,
    };
    let (_workspace, mut suffix_and_case) = application_with_skills(
        vec![
            SkillDescriptor {
                id: SkillId::new("lint-id"),
                name: "lint".to_owned(),
                description: "General lint guidance".to_owned(),
                scope: None,
            },
            unicode.clone(),
        ],
        None,
    );
    suffix_and_case
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$lint.rs then $über".to_owned(),
        )))
        .expect("paste a suffix and Unicode case variant");
    let ApplicationTransition::CreateSession(created) = suffix_and_case
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("resolve only the exact Unicode marker")
    else {
        panic!("Unicode marker creates a Skill-bearing Prompt");
    };
    assert_eq!(created.prompt.skill_invocations.len(), 1);
    assert_eq!(created.prompt.skill_invocations[0].skill_id, unicode.id);
    assert_eq!(created.prompt.skill_invocations[0].name, "Über");
}

#[test]
fn skill_completion_follows_the_active_token_and_escape_dismisses_until_it_ends() {
    let (_workspace, mut application) = application_with_skills(
        vec![
            SkillDescriptor {
                id: SkillId::new("opaque-review-id"),
                name: "review".to_owned(),
                description: "Review the current change".to_owned(),
                scope: None,
            },
            SkillDescriptor {
                id: SkillId::new("price-shaped-id"),
                name: ".99".to_owned(),
                description: "A deliberately price-shaped fixture".to_owned(),
                scope: None,
            },
        ],
        None,
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "first line\n($rev".to_owned(),
        )))
        .expect("type a multiline Skill query after punctuation");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains(" Skills ")
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::DismissCompletion))
        .expect("dismiss the active Skill token");
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "i".to_owned(),
        )))
        .expect("paste into the dismissed token");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "e".to_owned(),
        )))
        .expect("continue typing after the paste");
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains(" Skills "),
        "Escape dismissal lasts for the active token"
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            " $rev".to_owned(),
        )))
        .expect("begin another Skill token");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains(" Skills "),
        "a later token gets its own completion"
    );

    for literal in ["word$rev", "${review}", "$(review)", "$12.00", "$.99"] {
        application
            .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
            .expect("clear the draft");
        application
            .handle_event(ApplicationEvent::Command(CommandId::InsertText(
                literal.to_owned(),
            )))
            .expect("type literal dollar text");
        assert!(
            !rendered_application_rows(&application)
                .join("\n")
                .contains(" Skills "),
            "{literal:?} is not a Skill query"
        );
    }
}

#[test]
fn unicode_equivalent_skill_names_are_ambiguous() {
    let (_workspace, mut application) = application_with_skills(
        vec![
            SkillDescriptor {
                id: SkillId::new("composed-id"),
                name: "İ".to_owned(),
                description: "Composed spelling".to_owned(),
                scope: None,
            },
            SkillDescriptor {
                id: SkillId::new("decomposed-id"),
                name: "i\u{307}".to_owned(),
                description: "Decomposed spelling".to_owned(),
                scope: None,
            },
        ],
        None,
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$i\u{307}".to_owned(),
        )))
        .expect("paste a Unicode-equivalent marker");

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("reject the ambiguous marker"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("multiple Skills named")
    );
}

#[test]
fn longest_exact_marker_is_accented_while_ambiguous_input_is_rejected_in_place() {
    let (_workspace, mut application) = application_with_skills(
        vec![
            SkillDescriptor {
                id: SkillId::new("lint-short"),
                name: "lint".to_owned(),
                description: "Lint broadly".to_owned(),
                scope: None,
            },
            SkillDescriptor {
                id: SkillId::new("lint-rust"),
                name: "lint.rs".to_owned(),
                description: "Lint Rust".to_owned(),
                scope: None,
            },
            SkillDescriptor {
                id: SkillId::new("review-personal"),
                name: "review".to_owned(),
                description: "Personal review".to_owned(),
                scope: Some("Personal".to_owned()),
            },
            SkillDescriptor {
                id: SkillId::new("review-workspace"),
                name: "Review".to_owned(),
                description: "Workspace review".to_owned(),
                scope: Some("Workspace".to_owned()),
            },
        ],
        None,
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "Run $lint.rs, then $review".to_owned(),
        )))
        .expect("paste exact and ambiguous markers");

    let buffer = rendered_application_buffer(&application, 80, 15);
    for (marker, color) in [("$lint.rs", Color::Cyan), ("$review", Color::Red)] {
        let cells = buffer
            .content()
            .windows(marker.len())
            .find(|window| window.iter().map(|cell| cell.symbol()).collect::<String>() == marker)
            .unwrap_or_else(|| panic!("rendered composer contains {marker}"));
        assert!(
            cells.iter().all(|cell| cell.fg == color),
            "{marker} uses its semantic marker treatment"
        );
    }

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("reject ambiguous Prompt"),
        ApplicationTransition::Continue
    );
    let rejected = rendered_application_rows(&application).join("\n");
    assert!(rejected.contains("Run $lint.rs, then $review"));
    assert!(rejected.contains("multiple Skills named `review`"));
}

#[test]
fn skill_bindings_follow_outside_edits_and_same_named_replacements_stay_stale() {
    let review = SkillDescriptor {
        id: SkillId::new("review-original"),
        name: "review".to_owned(),
        description: "Review the current change".to_owned(),
        scope: None,
    };
    let (_workspace, mut edited) = application_with_skills(vec![review.clone()], None);
    edited
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "Use $review now".to_owned(),
        )))
        .expect("paste a bound Prompt");
    for _ in 0.."Use $review now".chars().count() {
        edited
            .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
            .expect("move before the marker");
    }
    edited
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            ">".to_owned(),
        )))
        .expect("edit wholly before the marker");
    for _ in 0.."Use $review now".chars().count() {
        edited
            .handle_event(ApplicationEvent::Command(CommandId::MoveCursorRight))
            .expect("move after the marker");
    }
    edited
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "!".to_owned(),
        )))
        .expect("edit wholly after the marker");
    let ApplicationTransition::CreateSession(created) = edited
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit rebased binding")
    else {
        panic!("outside edits preserve a valid Skill Prompt");
    };
    assert_eq!(created.prompt.text, ">Use $review now!");
    assert_eq!(created.prompt.skill_invocations.len(), 1);
    assert_eq!(created.prompt.skill_invocations[0].marker.start, 5);
    assert_eq!(created.prompt.skill_invocations[0].marker.end, 12);

    let (workspace, mut stale) = application_with_skills(vec![review], None);
    stale
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$review keep this draft".to_owned(),
        )))
        .expect("paste a bound Prompt");
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };
    stale
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider,
                workspace: request.workspace,
                skills: vec![SkillDescriptor {
                    id: SkillId::new("review-replacement"),
                    name: "review".to_owned(),
                    description: "Different instructions".to_owned(),
                    scope: None,
                }],
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations: None,
                    supported_deliveries: vec![SkillPromptDelivery::Initial],
                },
                status: SkillCatalogStatus::Fresh { warning: None },
            },
        })
        .expect("replace the catalog identity");
    let stale_buffer = rendered_application_buffer(&stale, 80, 15);
    let marker = stale_buffer
        .content()
        .windows(7)
        .find(|window| window.iter().map(|cell| cell.symbol()).collect::<String>() == "$review")
        .expect("stale marker remains visible");
    assert!(marker.iter().all(|cell| cell.fg == Color::Red));
    assert_eq!(
        stale
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("reject stale binding"),
        ApplicationTransition::Continue
    );
    let rejected = rendered_application_rows(&stale).join("\n");
    assert!(rejected.contains("$review keep this draft"));
    assert!(rejected.contains("stale"));
}

#[test]
fn provider_limit_disables_new_choices_and_rejects_manual_over_limit_input() {
    let review = SkillDescriptor {
        id: SkillId::new("review-id"),
        name: "review".to_owned(),
        description: "Audit".to_owned(),
        scope: None,
    };
    let test = SkillDescriptor {
        id: SkillId::new("test-id"),
        name: "test".to_owned(),
        description: "Test the change".to_owned(),
        scope: None,
    };
    let (_workspace, mut application) = application_with_skills(vec![review, test], Some(1));
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$review ".to_owned(),
        )))
        .expect("paste the first distinct Skill");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "$tes".to_owned(),
        )))
        .expect("query another distinct Skill");
    let limited = rendered_application_rows(&application).join("\n");
    assert!(limited.contains("$test"));
    assert!(limited.contains("limit reached"));
    application
        .handle_event(ApplicationEvent::Command(
            CommandId::ConfirmSelectedCompletion,
        ))
        .expect("disabled choice cannot be selected");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "t".to_owned(),
        )))
        .expect("manual over-limit input remains editable");
    let exact_limit = rendered_application_rows(&application).join("\n");
    assert!(exact_limit.contains("$test"));
    assert!(
        exact_limit.contains("limit reached"),
        "an exact manually bound over-limit choice remains disabled"
    );
    application
        .handle_event(ApplicationEvent::Command(
            CommandId::ConfirmSelectedCompletion,
        ))
        .expect("exact disabled choice still cannot be selected");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("reject over-limit Prompt atomically"),
        ApplicationTransition::Continue
    );
    let rejected = rendered_application_rows(&application).join("\n");
    assert!(rejected.contains("$review $test"));
    assert!(
        rejected.contains("supports at most 1 distinct Skill"),
        "{rejected}"
    );
}

#[test]
fn repeated_skill_markers_keep_first_appearance_order_in_the_prompt() {
    let alpha = SkillDescriptor {
        id: SkillId::new("alpha-id"),
        name: "alpha".to_owned(),
        description: "Alpha guidance".to_owned(),
        scope: None,
    };
    let beta = SkillDescriptor {
        id: SkillId::new("beta-id"),
        name: "beta".to_owned(),
        description: "Beta guidance".to_owned(),
        scope: None,
    };
    let (_workspace, mut application) =
        application_with_skills(vec![alpha.clone(), beta.clone()], None);
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$beta then $alpha and $beta".to_owned(),
        )))
        .expect("paste repeated Skills");
    let ApplicationTransition::CreateSession(created) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit repeated Skills")
    else {
        panic!("valid repeated Skills create a Session");
    };
    assert_eq!(
        created
            .prompt
            .skill_invocations
            .iter()
            .map(|invocation| invocation.skill_id.clone())
            .collect::<Vec<_>>(),
        vec![beta.id.clone(), alpha.id, beta.id]
    );
    assert_eq!(created.prompt.text.matches('$').count(), 3);
}

#[test]
fn history_restoration_resolves_exact_skills_without_opening_completion() {
    let review = SkillDescriptor {
        id: SkillId::new("review-id"),
        name: "review".to_owned(),
        description: "Review guidance".to_owned(),
        scope: None,
    };
    let (workspace, mut application) = application_with_skills(vec![review.clone()], None);
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$review".to_owned(),
        )))
        .expect("paste the first Prompt");
    let ApplicationTransition::CreateSession(created) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the first Prompt")
    else {
        panic!("first Prompt creates a Session");
    };
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        created.prompt.id,
        &created.prompt.text,
        workspace.path(),
    );
    snapshot.session.agent_selection = Some(AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-fixture"),
        options: Vec::new(),
    });
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(snapshot)))
        .expect("enter the created Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryPrevious))
        .expect("restore the prior Prompt");
    let restored = rendered_application_rows(&application).join("\n");
    assert!(restored.contains("$review"));
    assert!(
        !restored.contains(" Skills "),
        "history restoration must not open completion"
    );
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("resubmit restored Prompt")
    else {
        panic!("restored Prompt is admitted to the Session");
    };
    assert_eq!(request.prompt.skill_invocations.len(), 1);
    assert_eq!(request.prompt.skill_invocations[0].skill_id, review.id);
}

#[test]
fn intersecting_a_binding_invalidates_it_and_unknown_dollar_tokens_stay_literal() {
    let review = SkillDescriptor {
        id: SkillId::new("review-id"),
        name: "review".to_owned(),
        description: "Review guidance".to_owned(),
        scope: None,
    };
    let (_workspace, mut application) = application_with_skills(vec![review], None);
    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$unknown and $review".to_owned(),
        )))
        .expect("paste unknown and known markers");
    application
        .handle_event(ApplicationEvent::Command(CommandId::DeleteBackward))
        .expect("edit inside the known marker");
    let buffer = rendered_application_buffer(&application, 80, 15);
    for literal in ["$unknown", "$revie"] {
        let cells = buffer
            .content()
            .windows(literal.len())
            .find(|window| window.iter().map(|cell| cell.symbol()).collect::<String>() == literal)
            .unwrap_or_else(|| panic!("rendered composer contains {literal}"));
        assert!(
            cells.iter().all(|cell| cell.fg == Color::Reset),
            "unknown and incomplete dollar text keeps normal styling"
        );
    }
    let ApplicationTransition::CreateSession(created) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit literal dollar text")
    else {
        panic!("unknown and invalidated markers remain an ordinary Prompt");
    };
    assert_eq!(created.prompt.text, "$unknown and $revie");
    assert!(created.prompt.skill_invocations.is_empty());
}

#[test]
fn rejected_admission_restores_the_complete_skill_bearing_draft() {
    let review = SkillDescriptor {
        id: SkillId::new("review-id"),
        name: "review".to_owned(),
        description: "Review guidance".to_owned(),
        scope: None,
    };
    let (workspace, mut application) = application_with_skills(vec![review.clone()], None);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Initial Prompt".to_owned(),
        )))
        .expect("type the first Prompt");
    let ApplicationTransition::CreateSession(initial) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the first Prompt")
    else {
        panic!("first Prompt creates a Session");
    };
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        initial.prompt.id,
        &initial.prompt.text,
        workspace.path(),
    );
    snapshot.session.agent_selection = initial.agent_selection;
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(snapshot)))
        .expect("enter the created Session");

    application
        .handle_event(ApplicationEvent::Command(CommandId::PasteText(
            "$review preserve every marker".to_owned(),
        )))
        .expect("paste a Skill-bearing Prompt");
    let ApplicationTransition::AdmitPrompt { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit Skill-bearing Prompt")
    else {
        panic!("Session Prompt begins admission");
    };
    application
        .handle_event(ApplicationEvent::PromptAdmissionFailed {
            prompt_id: request.prompt.id,
            error: "catalog changed during admission".to_owned(),
        })
        .expect("reject admission atomically");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("$review preserve every marker")
    );
    let ApplicationTransition::AdmitPrompt { request: retry, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("retry the restored Prompt")
    else {
        panic!("restored Prompt remains recoverable");
    };
    assert_eq!(retry.prompt.id, request.prompt.id);
    assert_eq!(retry.prompt.text, request.prompt.text);
    assert_eq!(retry.prompt.skill_invocations.len(), 1);
    assert_eq!(retry.prompt.skill_invocations[0].skill_id, review.id);
}

#[test]
fn skill_completion_shows_live_catalog_states_and_retries_failed_discovery() {
    let workspace = workspace_dir();
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-fixture"),
        options: Vec::new(),
    };
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(Uuid::new_v4(), 42).with_landing_agent_selection(Some(selection)),
        )))
        .expect("connect application");
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "$".to_owned(),
        )))
        .expect("type a Skill marker while prefetch is pending");
    let loading = rendered_application_rows(&application).join("\n");
    assert!(loading.contains(" Skills "));
    assert!(loading.contains("Loading Skills"));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
            .expect("clear the pending marker"),
        ApplicationTransition::Continue
    );

    let stale_skill = SkillDescriptor {
        id: SkillId::new("stale-review-id"),
        name: "review".to_owned(),
        description: "Review the current change".to_owned(),
        scope: Some("Workspace".to_owned()),
    };
    application
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider.clone(),
                workspace: request.workspace.clone(),
                skills: vec![stale_skill],
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations: None,
                    supported_deliveries: vec![SkillPromptDelivery::Initial],
                },
                status: SkillCatalogStatus::Stale {
                    message: "refresh failed".to_owned(),
                },
            },
        })
        .expect("load stale Skill Catalog");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InsertText(
                "$".to_owned(),
            )))
            .expect("typing a Skill marker retries stale discovery"),
        ApplicationTransition::RefreshSkills(request.clone())
    );
    let stale = rendered_application_rows(&application).join("\n");
    assert!(
        stale.contains("$review"),
        "stale entries remain presentational"
    );
    assert!(stale.contains("refresh failed"));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(
                CommandId::ConfirmSelectedCompletion,
            ))
            .expect("stale row cannot be confirmed"),
        ApplicationTransition::Continue
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InsertText(
                "r".to_owned(),
            )))
            .expect("ordinary editing does not trigger another retry"),
        ApplicationTransition::Continue
    );

    application
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider,
                workspace: request.workspace,
                skills: vec![SkillDescriptor {
                    id: SkillId::new("fresh-review-id"),
                    name: "review".to_owned(),
                    description: "Review the current change".to_owned(),
                    scope: Some("Workspace".to_owned()),
                }],
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations: None,
                    supported_deliveries: vec![SkillPromptDelivery::Initial],
                },
                status: SkillCatalogStatus::Fresh {
                    warning: Some("1 invalid Skill was skipped".to_owned()),
                },
            },
        })
        .expect("load partial Skill Catalog");
    let partial = rendered_application_rows(&application).join("\n");
    assert!(partial.contains("$review"));
    assert!(partial.contains("1 invalid Skill was skipped"));
}

#[test]
fn composer_cursor_wraps_at_the_right_edge_and_remains_visible_when_scrolled() {
    let mut wrapped = Application::default();
    wrapped
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "x".repeat(70),
        )))
        .expect("fill the composer's content row");
    let wrapped_buffer = rendered_application_buffer(&wrapped, 80, 30);
    let first = text_position(&wrapped_buffer, "xxxx");
    assert_eq!(prompt_block_height(&buffer_rows(&wrapped_buffer)), 4);
    assert_eq!(
        rendered_application_cursor_at(&wrapped, 80, 30),
        Position::new(first.0, first.1 + 1),
        "an insertion point after a full row belongs at the start of the next row"
    );

    let mut scrolled = Application::default();
    scrolled
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            (1..=20)
                .map(|line| format!("line{line:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )))
        .expect("type a Prompt taller than the composer cap");
    let scrolled_buffer = rendered_application_buffer(&scrolled, 80, 30);
    let final_line = text_position(&scrolled_buffer, "line20");
    assert_eq!(
        rendered_application_cursor_at(&scrolled, 80, 30),
        Position::new(final_line.0 + 6, final_line.1)
    );
}

#[test]
fn semantic_bindings_preserve_multiline_unicode_input_and_clear_before_exit() {
    let mut application = Application::default();
    let paste = command_for_terminal_event(InputEvent::Paste("a🙂β".to_owned()))
        .expect("map bracketed paste to editor input");
    application
        .handle_event(ApplicationEvent::Command(paste))
        .expect("paste Unicode text");
    application
        .handle_event(ApplicationEvent::Command(
            command_for_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Left,
                KeyModifiers::NONE,
            )))
            .expect("map left cursor movement"),
        ))
        .expect("move over one Unicode character");
    application
        .handle_event(ApplicationEvent::Command(CommandId::DeleteBackward))
        .expect("delete the preceding Unicode character");

    for event in [
        InputEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        InputEvent::Paste("x\ny".to_owned()),
    ] {
        application
            .handle_event(ApplicationEvent::Command(
                command_for_terminal_event(event).expect("map multiline editor input"),
            ))
            .expect("edit multiline Prompt");
    }
    let screen = rendered_application_rows(&application).join("\n");
    assert!(screen.contains("a"));
    assert!(screen.contains("x"));
    assert!(screen.contains("yβ"));
    assert!(!screen.contains('🙂'));
    assert!(screen.contains("Enter submit"));
    assert!(screen.contains("Shift+Enter newline"));

    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))),
        Some(CommandId::SubmitSteer)
    );
    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        ))),
        Some(CommandId::InsertNewline)
    );
    assert_eq!(
        command_for_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        ))),
        Some(CommandId::InsertNewline)
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
            .expect("clear non-empty composer"),
        ApplicationTransition::Continue
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Type a Prompt")
    );
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
            .expect("exit with an empty composer"),
        ApplicationTransition::Exit
    );
}

#[test]
fn composer_grows_to_one_third_of_the_terminal_then_scrolls_internally() {
    let mut one_line = Application::default();
    one_line
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "one line".to_owned(),
        )))
        .expect("type short Prompt");
    let one_line_rows = rendered_application_rows_at(&one_line, 80, 30);
    assert_eq!(prompt_block_height(&one_line_rows), 3);

    let mut four_lines = Application::default();
    four_lines
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "one\ntwo\nthree\nfour".to_owned(),
        )))
        .expect("type multiline Prompt");
    let four_line_rows = rendered_application_rows_at(&four_lines, 80, 30);
    assert_eq!(prompt_block_height(&four_line_rows), 6);

    let mut long = Application::default();
    long.handle_event(ApplicationEvent::Command(CommandId::InsertText(
        (1..=20)
            .map(|line| format!("line{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )))
    .expect("type long Prompt");
    let long_rows = rendered_application_rows_at(&long, 80, 30);
    let long_screen = long_rows.join("\n");
    assert_eq!(prompt_block_height(&long_rows), 12);
    assert!(long_screen.contains("line20"));
    assert!(!long_screen.contains("line01"));
}

#[test]
fn text_entered_while_the_first_session_is_created_becomes_its_draft() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Initial Prompt".to_owned(),
        )))
        .expect("type initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit initial Prompt")
    else {
        panic!("landing submission should create a Session");
    };
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "next Prompt".to_owned(),
        )))
        .expect("begin the next Prompt while creation is pending");
    application
        .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
        .expect("move the pending landing draft cursor");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            failed_session_snapshot(
                SessionId::new(),
                request.prompt.id,
                &request.prompt.text,
                workspace.path(),
            ),
        )))
        .expect("enter created Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "!".to_owned(),
        )))
        .expect("edit the migrated Session draft at its preserved cursor");

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("next Promp!t")
    );
}

#[test]
fn multiline_history_is_boundary_aware_and_session_drafts_keep_their_cursor() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path());
    let (first_session, first_snapshot) = enter_session(&mut application, workspace.path());
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "top\nbottom".to_owned(),
        )))
        .expect("type multiline draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryPrevious))
        .expect("move within multiline draft before navigating history");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("bottom")
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryPrevious))
        .expect("navigate history at first-line boundary");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Initial Prompt")
    );
    application
        .handle_event(ApplicationEvent::Command(CommandId::HistoryNext))
        .expect("restore multiline draft from history navigation");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("bottom")
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::ClearOrExit))
        .expect("clear first Session draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "ac".to_owned(),
        )))
        .expect("type first Session draft");
    application
        .handle_event(ApplicationEvent::Command(CommandId::MoveCursorLeft))
        .expect("place first Session cursor between characters");

    let second_session = SessionId::new();
    let second_snapshot = failed_session_snapshot(
        second_session,
        PromptId::new(),
        "Second Session Prompt",
        workspace.path(),
    );
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            second_snapshot,
        )))
        .expect("switch to second Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "second draft".to_owned(),
        )))
        .expect("type second Session draft");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            first_snapshot,
        )))
        .expect("switch back to first Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "b".to_owned(),
        )))
        .expect("insert at restored first Session cursor");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("abc")
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
            failed_session_snapshot(
                second_session,
                PromptId::new(),
                "Second Session Prompt",
                workspace.path(),
            ),
        )))
        .expect("return to second Session");
    let second_screen = rendered_application_rows(&application).join("\n");
    assert!(second_screen.contains("second draft"));
    assert!(!second_screen.contains("abc"));
    assert_ne!(first_session, second_session);
}

fn prompt_block_height(rows: &[String]) -> usize {
    let top = rows
        .iter()
        .position(|row| row.contains('┌') && row.contains("Prompt"))
        .expect("Prompt block top border is rendered");
    let bottom = rows
        .iter()
        .enumerate()
        .skip(top + 1)
        .find_map(|(index, row)| row.contains('└').then_some(index))
        .expect("Prompt block bottom border is rendered");
    bottom - top + 1
}

fn application_with_skills(
    skills: Vec<SkillDescriptor>,
    max_distinct_invocations: Option<u32>,
) -> (WorkspaceDir, Application) {
    let workspace = workspace_dir();
    let selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-fixture"),
        options: Vec::new(),
    };
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(Uuid::new_v4(), 42).with_landing_agent_selection(Some(selection)),
        )))
        .expect("connect application");
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
    };
    application
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider,
                workspace: request.workspace,
                skills,
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations,
                    supported_deliveries: vec![SkillPromptDelivery::Initial],
                },
                status: SkillCatalogStatus::Fresh { warning: None },
            },
        })
        .expect("load Skill Catalog");
    (workspace, application)
}
