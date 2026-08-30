//! Composer input: cursor placement, growth, editing bindings, and history.

use crate::support::{
    WorkspaceDir, buffer_rows, connected_application, enter_active_session, enter_session,
    failed_session_snapshot, rendered_application_buffer, rendered_application_cursor_at,
    rendered_application_rows, rendered_application_rows_at, text_position, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::Color;
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        AgentSelection, Cost, CostBasis, ModelId, NativeMeter, PromptId, ProviderId, SessionChange,
        SessionId, SessionRevision, SessionUpdate, SkillCatalog, SkillCatalogCapabilities,
        SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor, SkillId, SkillPromptDelivery,
        Turn, TurnId, TurnStatus, Usage, UsageTotal, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, command_for_terminal_event,
    },
};
use uuid::Uuid;

use crate::support::ready_health;

#[test]
fn session_composer_footer_sums_persisted_usage_across_all_turn_outcomes() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Measure the whole Session",
        workspace.path(),
    );
    snapshot.turns[0].usage = Some(Usage {
        fresh_input_tokens: Some(10_000),
        cache_read_tokens: Some(100_000),
        output_tokens: Some(5_000),
        reasoning_tokens: Some(3_000),
        native_meter: NativeMeter::from_units(7.25),
        ..Usage::default()
    });
    snapshot.turns[0].cost = Cost::from_usd(0.31);
    snapshot.turns[0].cost_basis = Some(CostBasis::Reported);
    snapshot.turns.push(Turn {
        id: TurnId::new(),
        prompt_id: Some(snapshot.prompts[0].id),
        agent: None,
        status: TurnStatus::Interrupted,
        started_at: None,
        settled_at: None,
        usage: Some(Usage {
            fresh_input_tokens: Some(10_000),
            cache_write_tokens: Some(50_000),
            output_tokens: Some(8_000),
            reasoning_tokens: Some(2_000),
            ..Usage::default()
        }),
        cost: Cost::from_usd(0.52),
        cost_basis: Some(CostBasis::Reported),
    });

    let mut current_client = connected_application(workspace.path());
    current_client
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("open the measured Session");
    let current_screen = rendered_application_rows(&current_client).join("\n");
    let mut reloaded_client = Application::new(workspace.path());
    reloaded_client
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("restore the persisted Session in a fresh client");
    let reloaded_screen = rendered_application_rows(&reloaded_client).join("\n");

    for (client, screen) in [
        ("current", current_screen.as_str()),
        ("reloaded", reloaded_screen.as_str()),
    ] {
        assert!(
            screen.contains("38K · $0.83"),
            "the {client} client totals failed and interrupted Turns from the snapshot: {screen}"
        );
        assert!(
            !screen.contains("188K"),
            "cache traffic is excluded from the blended total: {screen}"
        );
        assert!(
            !screen.contains("7.25"),
            "the Provider-native premium-request meter has no footer surface: {screen}"
        );
    }
}

#[test]
fn session_composer_footer_updates_with_active_turn_usage_and_hides_unknown_cost() {
    let workspace = workspace_dir();
    let mut application = Application::new(workspace.path());
    let (session_id, snapshot, turn_id) = enter_active_session(&mut application, workspace.path());
    let without_usage = rendered_application_rows(&application).join("\n");
    assert!(!without_usage.contains("tokens"));
    assert!(!without_usage.contains('$'));

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::TurnUsageChanged {
                    turn_id,
                    usage: Usage {
                        fresh_input_tokens: Some(1_200),
                        cache_read_tokens: Some(8_000),
                        output_tokens: Some(900),
                        reasoning_tokens: Some(2_100),
                        ..Usage::default()
                    },
                    cost: None,
                    cost_basis: None,
                }],
            },
        )))
        .expect("record active Turn Usage");

    let updated = rendered_application_rows(&application).join("\n");
    assert!(
        updated.contains("4.2K"),
        "the footer updates from the shared Session projection: {updated}"
    );
    assert!(!updated.contains("12.2K"));
    assert!(
        !updated.contains("$0.00"),
        "unknown Cost is absent rather than fabricated as zero: {updated}"
    );
}

#[test]
fn session_composer_footer_counts_the_subagent_subtree_the_server_rolled_up() {
    let workspace = workspace_dir();
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Delegate the reading",
        workspace.path(),
    );
    let session_id = snapshot.session.id;
    snapshot.turns[0].usage = Some(Usage {
        fresh_input_tokens: Some(10_000),
        output_tokens: Some(5_000),
        ..Usage::default()
    });
    snapshot.turns[0].cost = Cost::from_usd(0.31);
    snapshot.turns[0].cost_basis = Some(CostBasis::Reported);

    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("open the delegating Session");
    let before = rendered_application_rows(&application).join("\n");
    assert!(
        before.contains("15K · $0.31"),
        "the footer starts on what the Session itself consumed: {before}"
    );

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(snapshot.revision.0 + 1),
                changes: vec![SessionChange::SubagentUsageChanged {
                    subagent_usage: Some(UsageTotal {
                        fresh_input_tokens: Some(4_000),
                        cache_read_tokens: Some(60_000),
                        output_tokens: Some(1_000),
                        cost: Cost::from_usd(0.12),
                        ..UsageTotal::default()
                    }),
                }],
            },
        )))
        .expect("roll the Subagent subtree up into the Session");

    let after = rendered_application_rows(&application).join("\n");
    assert!(
        after.contains("20K · $0.43"),
        "delegated work joins the total the footer states: {after}"
    );
    assert!(
        !after.contains("80K"),
        "a Subagent's cache traffic is left out of the blended total too: {after}"
    );
}

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
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
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
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
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
fn composer_text_keeps_a_column_of_air_inside_the_prompt_border() {
    let mut application = Application::default();
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "zebra".to_owned(),
        )))
        .expect("type a short Prompt");
    let buffer = rendered_application_buffer(&application, 80, 30);
    let rows = buffer_rows(&buffer);
    let block = prompt_block(&rows);
    let typed = text_position(&buffer, "zebra");
    assert_eq!(
        typed.0,
        block.left + 2,
        "typed text starts a column inside the left border: {}",
        rows[block.top + 1]
    );
    assert_eq!(
        rendered_application_cursor_at(&application, 80, 30),
        Position::new(block.left + 7, typed.1),
        "the caret follows the text past the margin"
    );

    let mut filled = Application::default();
    filled
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "y".repeat(200),
        )))
        .expect("type a Prompt wider than the composer");
    let filled_buffer = rendered_application_buffer(&filled, 80, 30);
    let filled_rows = buffer_rows(&filled_buffer);
    let first = text_position(&filled_buffer, "yyyy").1;
    let row = filled_rows[usize::from(first)].chars().collect::<Vec<_>>();
    assert_eq!(
        row[usize::from(block.right) - 1],
        ' ',
        "a filled row stops a column short of the right border: {}",
        filled_rows[usize::from(first)]
    );
    assert_eq!(
        row[usize::from(block.right) - 2],
        'y',
        "the margin is one column, not a wider gutter: {}",
        filled_rows[usize::from(first)]
    );
}

#[test]
fn composer_moves_whole_words_to_the_next_row_and_splits_only_oversized_ones() {
    let prompt = concat!(
        "Suru wraps the composer by whole words so that no ordinary word ",
        "is ever cut across two rendered rows of the Prompt block"
    );
    let mut application = Application::default();
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            prompt.to_owned(),
        )))
        .expect("type a Prompt that outgrows one row");
    let buffer = rendered_application_buffer(&application, 80, 30);
    let rows = buffer_rows(&buffer);
    let block = prompt_block(&rows);
    let content = rows
        .iter()
        .skip(block.top + 1)
        .take_while(|row| !row.contains('└'))
        .map(|row| {
            row.chars()
                .skip(usize::from(block.left) + 1)
                .take(usize::from(block.right - block.left) - 1)
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(
        content.len() > 1,
        "the Prompt is long enough to wrap: {content:?}"
    );
    for word in prompt.split_whitespace() {
        assert!(
            content
                .iter()
                .any(|row| row.split_whitespace().any(|rendered| rendered == word)),
            "the word {word:?} stays whole on one row: {content:?}"
        );
    }

    let mut oversized = Application::default();
    let word = "z".repeat(100);
    oversized
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(format!(
            "hello {word}"
        ))))
        .expect("type a word too wide for a row of its own");
    let oversized_buffer = rendered_application_buffer(&oversized, 80, 30);
    let oversized_rows = buffer_rows(&oversized_buffer);
    let block = prompt_block(&oversized_rows);
    assert!(
        oversized_rows[block.top + 1].contains("hello")
            && !oversized_rows[block.top + 1].contains('z'),
        "the oversized word moves off the row it does not fit: {}",
        oversized_rows[block.top + 1]
    );
    assert_eq!(
        oversized_rows[block.top + 2]
            .chars()
            .skip(usize::from(block.left) + 2)
            .take(block.content_width())
            .collect::<String>(),
        "z".repeat(block.content_width()),
        "a word with nowhere to move to is split within, filling the row"
    );
}

#[test]
fn composer_cursor_wraps_at_the_right_edge_and_remains_visible_when_scrolled() {
    let mut wrapped = Application::default();
    let empty = rendered_application_buffer(&wrapped, 80, 30);
    let content_width = prompt_block(&buffer_rows(&empty)).content_width();
    wrapped
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "x".repeat(content_width),
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
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
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
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            second_snapshot,
        )))
        .expect("switch to second Session");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "second draft".to_owned(),
        )))
        .expect("type second Session draft");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
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
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
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

/// Where the composer's Prompt block sits on screen.
struct PromptBlock {
    left: u16,
    right: u16,
    top: usize,
}

impl PromptBlock {
    /// The columns the Prompt's text is laid out over: the block, less its two
    /// borders and the column of air kept inside each.
    fn content_width(&self) -> usize {
        usize::from(self.right - self.left) - 3
    }
}

fn prompt_block(rows: &[String]) -> PromptBlock {
    let top = rows
        .iter()
        .position(|row| row.contains('┌') && row.contains("Prompt"))
        .expect("Prompt block top border is rendered");
    let left = rows[top]
        .chars()
        .position(|character| character == '┌')
        .expect("Prompt block has a left border") as u16;
    let right = rows[top]
        .chars()
        .position(|character| character == '┐')
        .expect("Prompt block has a right border") as u16;
    PromptBlock { left, right, top }
}

fn prompt_block_height(rows: &[String]) -> usize {
    let top = prompt_block(rows).top;
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
