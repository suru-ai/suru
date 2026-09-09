//! Landing execution navigation through semantic commands and Server responses.
use crate::support::{
    deliver_settings, fixture_instance_id, ready_health, rendered_application_rows_at,
    selector_label, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};
use suru::{
    managed_client::ManagedEvent,
    protocol::*,
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};

struct Layout {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    main: PathBuf,
    linked: PathBuf,
    nested: PathBuf,
    context: ResolvedWorkspace,
}
impl Layout {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = suru::paths::canonical(temporary.path()).unwrap();
        let main = root.join("main");
        let linked = root.join("linked");
        let nested = linked.join("nested");
        std::fs::create_dir(&main).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let metadata = main.join(".git");
        let id = RepositoryId::from_metadata("git", &metadata);
        let repository = Repository {
            id: id.clone(),
            system: "git".to_owned(),
            metadata_directory: metadata,
            location: RepositoryLocation::Main { root: main.clone() },
            availability: SourceControlAvailability::Available,
            capabilities: SourceControlCapabilities::discovery_only(),
        };
        let workspace = Workspace {
            id: id.workspace_id(),
            path: main.clone(),
            repository: Some(repository),
            source_control: SourceControlAvailability::Available,
        };
        let checkouts = [(&main, CheckoutKind::Main), (&linked, CheckoutKind::Linked)]
            .into_iter()
            .map(|(root, kind)| CheckoutSummary {
                association: CheckoutAssociation {
                    recovery_revision: None,
                    id: CheckoutId::from_root(&id, root),
                    repository: id.clone(),
                    root: root.clone(),
                    kind,
                },
                revision: Some(if kind == CheckoutKind::Main {
                    CheckoutRevision::Branch {
                        name: "main".to_owned(),
                        commit: Some("1234567890abcdef".to_owned()),
                    }
                } else {
                    CheckoutRevision::Detached {
                        commit: "abcdef0123456789".to_owned(),
                    }
                }),
                availability: SourceControlAvailability::Available,
            })
            .collect::<Vec<_>>();
        let context = ResolvedWorkspace {
            workspace,
            execution_status: ExecutionDirectoryStatus::Available,
            execution_directory: Some(ExecutionDirectory {
                path: nested.clone(),
            }),
            checkout: Some(checkouts[1].association.clone()),
            checkouts,
        };
        Self {
            _temporary: temporary,
            root,
            main,
            linked,
            nested,
            context,
        }
    }
    /// The Health an owning Server would answer for this Layout, naming the
    /// fixture root as the home it resolved.
    ///
    /// Declaring a home is what keeps these renderings independent of where a
    /// platform puts its temp directory. `tempfile::tempdir` answers
    /// `/tmp/.tmpXXXXXX` on Linux but `C:\Users\you\AppData\Local\Temp\.tmpXXXXXX`
    /// on Windows — roughly forty columns rather than fifteen — and the
    /// landing's Workspace line is a fixed 72 columns wide however wide the
    /// terminal is, so rendering wider cannot buy it room. An unabbreviated
    /// Windows temp path eats the columns the hint beside it needs and
    /// truncates the hint away entirely. Rooting the Server's home at the
    /// fixture root abbreviates every path here to a `~`-relative label —
    /// `~/main`, `~/linked/nested`, spelled with the platform's own separator —
    /// short and the same shape on all three platforms, so these tests assert
    /// the hint rather than the accident of a temp path's length.
    fn health(&self, instance_id: uuid::Uuid, pid: u32) -> Health {
        ready_health(instance_id, pid)
            .with_workspace_paths(WorkspacePaths::from_home(Some(&self.root)))
    }
    fn at(&self, path: &Path) -> ResolvedWorkspace {
        let mut context = self.context.clone();
        context.execution_directory = Some(ExecutionDirectory {
            path: path.to_owned(),
        });
        context.checkout = context
            .checkouts
            .iter()
            .rev()
            .find(|checkout| path.starts_with(&checkout.association.root))
            .map(|checkout| checkout.association.clone());
        context
    }
    fn app(&self) -> Application {
        let mut app = Application::new(&self.nested, Default::default());
        let transition = app
            .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
                self.health(fixture_instance_id(), 42_424)
                    .with_landing_agent_selection(Some(AgentSelection {
                        provider: ProviderId::new("codex"),
                        model: ModelId::new("test"),
                        options: vec![],
                    })),
            )))
            .unwrap();
        answer(&mut app, transition, self.context.clone());
        app
    }
}
fn answer(app: &mut Application, transition: ApplicationTransition, context: ResolvedWorkspace) {
    let ApplicationTransition::ResolveWorkspace {
        outlook,
        surface,
        request_id,
        ..
    } = transition
    else {
        panic!("expected owning Server resolution: {transition:?}")
    };
    app.handle_event(ApplicationEvent::WorkspaceResolved {
        outlook,
        surface,
        request_id,
        result: Ok(context),
    })
    .unwrap();
}
fn command(app: &mut Application, command: SemanticCommandId) -> ApplicationTransition {
    app.handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
        command,
    )))
    .unwrap()
}
fn key(app: &mut Application, key: KeyCode) -> ApplicationTransition {
    app.handle_terminal_event(InputEvent::Key(KeyEvent::new(key, KeyModifiers::NONE)))
        .unwrap()
}
fn text(app: &Application) -> String {
    rendered_application_rows_at(app, 160, 30).join("\n")
}
fn open(app: &mut Application, context: ResolvedWorkspace) {
    let transition = command(app, SemanticCommandId::WorktreeList);
    answer(app, transition, context);
}
fn summary(context: &ResolvedWorkspace, title: &str) -> SessionListItem {
    SessionListItem::Readable(Box::new(SessionSummary {
        checkout_state: None,
        session: Session {
            id: SessionId::new(),
            workspace: context.workspace.clone(),
            execution_directory: context.execution_directory.clone().unwrap(),
            checkout: context.checkout.clone(),
            context_fill: None,
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Idle,
            working_since: None,
            parent: None,
        },
        title: title.to_owned(),
        emoji: None,
        settled_at: None,
        standing_inputs: Default::default(),
        total_usage: None,
        created_at: SessionTimestamp(1),
        updated_at: SessionTimestamp(2),
    }))
}
fn pick_other_workspace(
    app: &mut Application,
    contexts: &[ResolvedWorkspace],
) -> ApplicationTransition {
    let ApplicationTransition::ListSessions(request) =
        command(app, SemanticCommandId::WorkspaceList)
    else {
        panic!("list Workspace choices")
    };
    app.handle_event(ApplicationEvent::SessionsListed {
        request,
        sessions: contexts
            .iter()
            .map(|context| summary(context, "Session"))
            .collect(),
    })
    .unwrap();
    key(app, KeyCode::Down);
    key(app, KeyCode::Enter)
}

#[test]
fn landing_worktree_selection_uses_root_and_cancel_ignores_late_server_response() {
    let layout = Layout::new();
    let mut app = layout.app();
    let loading = command(&mut app, SemanticCommandId::WorktreeList);
    assert!(text(&app).contains("Loading Worktrees"));
    assert!(matches!(
        key(&mut app, KeyCode::Esc),
        ApplicationTransition::CancelWorkspaceResolution(_)
    ));
    answer(&mut app, loading, layout.at(&layout.main));
    assert!(!text(&app).contains(" Worktrees "));
    assert!(text(&app).contains("nested"));
    open(&mut app, layout.context.clone());
    let rows = text(&app);
    assert!(rows.contains("Current:"));
    assert!(rows.contains("detached abcdef01"));
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    let transition = key(&mut app, KeyCode::Enter);
    let ApplicationTransition::ResolveWorkspace { request, .. } = &transition else {
        panic!("select checkout on Server")
    };
    assert_eq!(
        request.checkout_id,
        Some(layout.context.checkouts[1].association.id.clone())
    );
    assert_eq!(
        request.workspace_id,
        Some(layout.context.workspace.id.clone())
    );
    answer(&mut app, transition, layout.at(&layout.linked));
    type_terminal_text(&mut app, "Use this checkout");
    let ApplicationTransition::CreateSession(request) = key(&mut app, KeyCode::Enter) else {
        panic!("start new Session")
    };
    assert_eq!(request.execution_directory.path, layout.linked);
}

#[test]
fn explicit_subdirectory_keeps_draft_and_updates_destination_skills_and_relative_path_base() {
    let layout = Layout::new();
    let mut app = layout.app();
    open(&mut app, layout.context.clone());
    for _ in 0..3 {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Enter);
    app.handle_terminal_event(InputEvent::Paste("../another directory".to_owned()))
        .unwrap();
    let transition = key(&mut app, KeyCode::Enter);
    let ApplicationTransition::ResolveWorkspace { request, .. } = &transition else {
        panic!("resolve explicit subdirectory")
    };
    assert_eq!(request.base, Some(layout.nested.clone()));
    assert_eq!(request.path, PathBuf::from("../another directory"));
    let destination = layout.linked.join("another directory");
    std::fs::create_dir(&destination).unwrap();
    answer(&mut app, transition, layout.at(&destination));
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        execution_directory: ExecutionDirectory {
            path: destination.clone(),
        },
    };
    app.handle_event(ApplicationEvent::SkillsListed {
        request: request.clone(),
        catalog: SkillCatalog {
            provider: request.provider.clone(),
            execution_directory: request.execution_directory.clone(),
            skills: vec![],
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: None,
                supported_deliveries: vec![SkillPromptDelivery::Initial],
            },
            status: SkillCatalogStatus::Stale {
                message: "Refresh destination".to_owned(),
            },
        },
    })
    .unwrap();
    let ApplicationTransition::RefreshSkills(request) = app
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "$".to_owned(),
        )))
        .unwrap()
    else {
        panic!("refresh destination Skills")
    };
    assert_eq!(request.execution_directory.path, destination);
    key(&mut app, KeyCode::Esc);
    let transition = deliver_settings(
        &mut app,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Shown,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    if let ApplicationTransition::ListSessions(request) = transition {
        app.handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![],
        })
        .unwrap();
    }
    crate::support::press_add_workspace(&mut app);
    type_terminal_text(&mut app, "child");
    let ApplicationTransition::ResolveWorkspace { request, .. } = key(&mut app, KeyCode::Enter)
    else {
        panic!("Sidebar resolves paths on owning Server")
    };
    assert_eq!(request.base, Some(destination));
}

#[test]
fn workspace_picker_remembers_exact_directory_and_missing_choice_without_changing_sidebar_scope() {
    let layout = Layout::new();
    let mut app = layout.app();
    let transition = deliver_settings(
        &mut app,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Shown,
                initial_scope: SidebarScope::CurrentWorkspace,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("load Sidebar")
    };
    app.handle_event(ApplicationEvent::SessionsListed {
        request,
        sessions: vec![summary(&layout.context, "Initial Session")],
    })
    .unwrap();
    let scope = selector_label(&rendered_application_rows_at(&app, 160, 30));
    let other = ResolvedWorkspace::directory(layout.main.parent().unwrap().join("other"));
    let transition = pick_other_workspace(&mut app, &[layout.context.clone(), other.clone()]);
    answer(&mut app, transition, other.clone());
    assert_eq!(
        selector_label(&rendered_application_rows_at(&app, 160, 30)),
        scope
    );
    let transition = pick_other_workspace(&mut app, &[layout.context.clone(), other]);
    let ApplicationTransition::ResolveWorkspace { request, .. } = &transition else {
        panic!("restore Workspace")
    };
    assert_eq!(
        request
            .remembered_execution_directory
            .as_ref()
            .unwrap()
            .path,
        layout.nested
    );
    let mut missing = layout.context.clone();
    missing.execution_status = ExecutionDirectoryStatus::Unavailable {
        reason: "Directory was removed".to_owned(),
    };
    answer(&mut app, transition, missing.clone());
    assert!(text(&app).contains("nested"));
    assert!(text(&app).contains("unavailable"));
    type_terminal_text(&mut app, "Keep this draft");
    assert_eq!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(text(&app).contains("Keep this draft"));
    open(&mut app, missing);
    key(&mut app, KeyCode::Down);
    let transition = key(&mut app, KeyCode::Enter);
    answer(&mut app, transition, layout.at(&layout.main));
    let ApplicationTransition::CreateSession(request) = key(&mut app, KeyCode::Enter) else {
        panic!("explicit root choice makes draft executable")
    };
    assert_eq!(request.execution_directory.path, layout.main);
    assert_eq!(request.prompt.text, "Keep this draft");
}

#[test]
fn unavailable_checkout_refuses_selection_and_keeps_current_directory() {
    let layout = Layout::new();
    let mut app = layout.app();
    let mut context = layout.context.clone();
    context.checkouts[0].availability = SourceControlAvailability::Unavailable {
        reason: "Main checkout is unreadable".to_owned(),
    };
    open(&mut app, context);
    key(&mut app, KeyCode::Down);
    assert_eq!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(text(&app).contains("Main checkout is unreadable"));
    key(&mut app, KeyCode::Esc);
    type_terminal_text(&mut app, "Keep using this directory");
    let ApplicationTransition::CreateSession(request) = key(&mut app, KeyCode::Enter) else {
        panic!("current directory stays selected")
    };
    assert_eq!(request.execution_directory.path, layout.nested);
}

#[test]
fn source_skill_binding_is_not_retargeted_to_another_worktree() {
    let layout = Layout::new();
    let mut app = layout.app();
    let catalog_request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        execution_directory: ExecutionDirectory {
            path: layout.nested.clone(),
        },
    };
    app.handle_event(ApplicationEvent::SkillsListed {
        request: catalog_request.clone(),
        catalog: SkillCatalog {
            provider: catalog_request.provider,
            execution_directory: catalog_request.execution_directory,
            skills: vec![SkillDescriptor {
                id: SkillId::new("source-review"),
                name: "review".to_owned(),
                description: "Review source".to_owned(),
                scope: None,
            }],
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: None,
                supported_deliveries: vec![SkillPromptDelivery::Initial],
            },
            status: SkillCatalogStatus::Fresh { warning: None },
        },
    })
    .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::InsertText(
        "Please $rev".to_owned(),
    )))
    .unwrap();
    app.handle_event(ApplicationEvent::Command(
        CommandId::ConfirmSelectedCompletion,
    ))
    .unwrap();
    open(&mut app, layout.context.clone());
    key(&mut app, KeyCode::Down);
    let transition = key(&mut app, KeyCode::Enter);
    answer(&mut app, transition, layout.at(&layout.main));
    assert_eq!(
        app.handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .unwrap(),
        ApplicationTransition::Continue
    );
    assert!(text(&app).contains("stale"));
    assert!(text(&app).contains("$review"));
}

#[test]
fn same_workspace_identity_on_another_origin_has_independent_execution_memory() {
    use suru::tui::WorkspaceResolutionSurface;
    let layout = Layout::new();
    let mut app = layout.app();
    crate::connecting::turn_to_studio(&mut app);
    app.handle_event(ApplicationEvent::WorkspaceResolved {
        outlook: Outlook::Remote("studio".to_owned()),
        surface: WorkspaceResolutionSurface::Outlook,
        request_id: 2,
        result: Ok(layout.at(&layout.main)),
    })
    .unwrap();
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    let ApplicationTransition::ResolveWorkspace {
        outlook, request, ..
    } = &transition
    else {
        panic!("Remote owns checkout discovery")
    };
    assert_eq!(*outlook, Outlook::Remote("studio".to_owned()));
    assert_eq!(
        request
            .remembered_execution_directory
            .as_ref()
            .unwrap()
            .path,
        layout.main
    );
    answer(&mut app, transition, layout.at(&layout.main));
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    let transition = key(&mut app, KeyCode::Enter);
    answer(&mut app, transition, layout.at(&layout.linked));
    command(&mut app, SemanticCommandId::ConnectOpen);
    app.handle_event(ApplicationEvent::RemotesListed(vec![Remote {
        name: "studio".to_owned(),
        fingerprint: "studio-fingerprint".to_owned(),
        addresses: vec!["10.0.0.8:7777".parse().unwrap()],
        status: RemoteStatus::Available,
    }]))
    .unwrap();
    key(&mut app, KeyCode::Up);
    assert!(matches!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::TurnOutlook {
            outlook: Outlook::Local,
            ..
        }
    ));
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    let ApplicationTransition::ResolveWorkspace {
        outlook, request, ..
    } = &transition
    else {
        panic!("Local context resolves locally")
    };
    assert_eq!(*outlook, Outlook::Local);
    assert_eq!(
        request
            .remembered_execution_directory
            .as_ref()
            .unwrap()
            .path,
        layout.nested
    );
    answer(&mut app, transition, layout.context.clone());
    assert!(text(&app).contains("nested"));
}

#[test]
fn bare_landing_offers_working_copies_and_choosing_one_makes_next_prompt_executable() {
    let layout = Layout::new();
    let mut bare = layout.context.clone();
    bare.workspace.repository.as_mut().unwrap().location = RepositoryLocation::Bare {
        root: layout.main.clone(),
    };
    bare.execution_directory = None;
    bare.execution_status = ExecutionDirectoryStatus::RequiresWorkingCopy;
    bare.checkout = None;
    let mut app = Application::new(&layout.main, Default::default());
    let transition = app
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            layout.health(fixture_instance_id(), 42_424),
        )))
        .unwrap();
    answer(&mut app, transition, bare.clone());
    assert!(text(&app).contains("Choose a working copy"));
    open(&mut app, bare);
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    let transition = key(&mut app, KeyCode::Enter);
    answer(&mut app, transition, layout.at(&layout.linked));
    type_terminal_text(&mut app, "Start in this working copy");
    let ApplicationTransition::CreateSession(request) = key(&mut app, KeyCode::Enter) else {
        panic!("selected working copy is executable")
    };
    assert_eq!(request.execution_directory.path, layout.linked);
}

#[test]
fn choosing_worktree_from_open_session_cannot_relocate_that_session() {
    let layout = Layout::new();
    let mut app = layout.app();
    let (id, _) = crate::support::enter_session(&mut app, &layout.nested);
    assert_eq!(
        command(&mut app, SemanticCommandId::WorktreeList),
        ApplicationTransition::Continue
    );
    assert!(!text(&app).contains(" Worktrees "));
    type_terminal_text(&mut app, "Continue in the original directory");
    let ApplicationTransition::AdmitPrompt { session, .. } = app
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .unwrap()
    else {
        panic!("existing Session keeps its execution context")
    };
    assert_eq!(session.session_id, id);
}

#[test]
fn cancelling_directory_resolution_does_not_adopt_a_late_destination() {
    let layout = Layout::new();
    let mut app = layout.app();
    open(&mut app, layout.context.clone());
    for _ in 0..3 {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Enter);
    type_terminal_text(&mut app, "somewhere else");
    let transition = key(&mut app, KeyCode::Enter);
    assert!(matches!(
        key(&mut app, KeyCode::Esc),
        ApplicationTransition::CancelWorkspaceResolution(_)
    ));
    answer(&mut app, transition, layout.at(&layout.main));
    key(&mut app, KeyCode::Esc);
    type_terminal_text(&mut app, "Keep current directory");
    let ApplicationTransition::CreateSession(request) = key(&mut app, KeyCode::Enter) else {
        panic!("current directory stays executable")
    };
    assert_eq!(request.execution_directory.path, layout.nested);
}

#[test]
fn worktree_chooser_during_initial_discovery_uses_exact_launch_path_until_server_knows_identity() {
    let layout = Layout::new();
    let mut app = Application::new(&layout.nested, Default::default());
    let initial = app
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            layout.health(fixture_instance_id(), 42_424),
        )))
        .unwrap();
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    let ApplicationTransition::ResolveWorkspace { request, .. } = &transition else {
        panic!("resolve launch Worktrees")
    };
    assert!(
        request.workspace_id.is_none(),
        "the Client's provisional path identity is not a Server Repository identity"
    );
    assert_eq!(request.path, layout.nested);
    answer(&mut app, transition, layout.context.clone());
    key(&mut app, KeyCode::Down);
    let selected = key(&mut app, KeyCode::Enter);
    let ApplicationTransition::ResolveWorkspace { request, .. } = &selected else {
        panic!("select discovered Worktree")
    };
    assert_eq!(
        request.workspace_id,
        Some(layout.context.workspace.id.clone())
    );
    answer(&mut app, selected, layout.at(&layout.main));
    answer(&mut app, initial, layout.context.clone());
    type_terminal_text(&mut app, "Use explicit choice");
    let ApplicationTransition::CreateSession(request) = key(&mut app, KeyCode::Enter) else {
        panic!("explicit Worktree choice wins over late startup reading")
    };
    assert_eq!(request.execution_directory.path, layout.main);
}

fn choose_new(app: &mut Application, layout: &Layout) {
    let mut context = layout.context.clone();
    context
        .workspace
        .repository
        .as_mut()
        .unwrap()
        .capabilities
        .create_checkout = SourceControlCapability::Available;
    open(app, context);
    key(app, KeyCode::Up); // Last row is the semantic new-Worktree intent.
    assert_eq!(key(app, KeyCode::Enter), ApplicationTransition::Continue);
}
fn prepared(layout: &Layout, request: &PrepareCheckoutRequest) -> PrepareCheckoutResult {
    let destination = layout.main.join(".suru-worktrees/test/prepared");
    std::fs::create_dir_all(&destination).unwrap();
    let mut location = layout.at(&destination);
    let repository = location.workspace.repository.clone().unwrap();
    location.checkout = Some(CheckoutAssociation {
        recovery_revision: None,
        id: CheckoutId::from_root(&repository.id, &destination),
        repository: repository.id.clone(),
        root: destination.clone(),
        kind: CheckoutKind::Linked,
    });
    PrepareCheckoutResult {
        preparation: PreparedCheckout {
            id: request.id,
            source: request.source.clone(),
            repository,
            destination: ExecutionDirectory { path: destination },
            plan: CheckoutPreparationPlan::Git {
                branch: "suru/prepared".to_owned(),
                source_commit: "abc".to_owned(),
            },
            checkout_created: true,
            ready: true,
            intended_session: SessionId::new(),
            admitted_session: None,
        },
        location: Some(location),
        error: None,
    }
}

#[test]
fn new_worktree_intent_is_deferred_cancelable_and_first_prompt_automatically_admits_at_prepared_root()
 {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    assert!(text(&app).contains("New Worktree on submit"));
    assert!(!layout.main.join(".suru-worktrees").exists());
    open(&mut app, layout.context.clone());
    key(&mut app, KeyCode::Enter); // Current cancels the intention.
    assert!(!text(&app).contains("New Worktree on submit"));
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Prepare and work");
    let ApplicationTransition::PrepareCheckout { prompt_id, request } =
        key(&mut app, KeyCode::Enter)
    else {
        panic!("first submit prepares")
    };
    assert_eq!(request.source.path, layout.nested);
    let result = prepared(&layout, &request);
    let destination = result.preparation.destination.clone();
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared { prompt_id, result })
        .unwrap()
    else {
        panic!("unbound Prompt automatically continues")
    };
    assert_eq!(create.preparation_id, Some(request.id));
    assert_eq!(create.execution_directory, destination);
    assert_eq!(create.prompt.id, prompt_id);
}

#[test]
fn preparation_failure_preserves_draft_and_id_and_late_results_cannot_replace_deliberate_choice() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Retry this draft");
    let ApplicationTransition::PrepareCheckout { prompt_id, request } =
        key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    let mut result = prepared(&layout, &request);
    result.error = Some("Submodule initialization failed; retry".to_owned());
    result.preparation.ready = false;
    app.handle_event(ApplicationEvent::CheckoutPrepared { prompt_id, result })
        .unwrap();
    assert!(text(&app).contains("Retry this draft"));
    assert!(text(&app).contains("Submodule initialization failed"));
    type_terminal_text(&mut app, " corrected");
    let ApplicationTransition::PrepareCheckout {
        prompt_id: retry_id,
        request: retry,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("retry")
    };
    assert_eq!(retry.id, request.id);
    assert_ne!(retry_id, prompt_id);
    open(&mut app, layout.at(&layout.main));
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.handle_event(ApplicationEvent::CheckoutPrepared {
            prompt_id: retry_id,
            result: prepared(&layout, &retry)
        })
        .unwrap(),
        ApplicationTransition::Continue
    );
    let ApplicationTransition::CreateSession(create) = key(&mut app, KeyCode::Enter) else {
        panic!("draft remains usable after changing destination")
    };
    assert_eq!(create.preparation_id, None);
    assert_eq!(create.execution_directory.path, layout.main);
}

fn offer_review(app: &mut Application, path: &Path, id: &str) {
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        execution_directory: ExecutionDirectory {
            path: path.to_owned(),
        },
    };
    app.handle_event(ApplicationEvent::SkillsListed {
        request: request.clone(),
        catalog: SkillCatalog {
            provider: request.provider,
            execution_directory: request.execution_directory,
            skills: vec![SkillDescriptor {
                id: SkillId::new(id),
                name: "review".to_owned(),
                description: "Review change".to_owned(),
                scope: None,
            }],
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: None,
                supported_deliveries: vec![SkillPromptDelivery::Initial],
            },
            status: SkillCatalogStatus::Fresh { warning: None },
        },
    })
    .unwrap();
}
#[test]
fn managed_preparation_requires_destination_skill_reselection_and_keeps_identity_after_correcting_draft()
 {
    let layout = Layout::new();
    let mut app = layout.app();
    offer_review(&mut app, &layout.nested, "source-review");
    app.handle_event(ApplicationEvent::Command(CommandId::InsertText(
        "Please $rev".to_owned(),
    )))
    .unwrap();
    app.handle_event(ApplicationEvent::Command(
        CommandId::ConfirmSelectedCompletion,
    ))
    .unwrap();
    choose_new(&mut app, &layout);
    let ApplicationTransition::PrepareCheckout { prompt_id, request } =
        key(&mut app, KeyCode::Enter)
    else {
        panic!("source bindings may prepare")
    };
    let result = prepared(&layout, &request);
    let destination = result.preparation.destination.clone();
    let ApplicationTransition::RefreshSkills(refresh) = app
        .handle_event(ApplicationEvent::CheckoutPrepared { prompt_id, result })
        .unwrap()
    else {
        panic!("explicit bindings require reselection")
    };
    assert_eq!(refresh.execution_directory, destination);
    assert!(text(&app).contains("$review"));
    assert_eq!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    offer_review(&mut app, &destination.path, "destination-review");
    app.handle_event(ApplicationEvent::Command(CommandId::SelectAll))
        .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::InsertText(
        "Please $rev".to_owned(),
    )))
    .unwrap();
    app.handle_event(ApplicationEvent::Command(
        CommandId::ConfirmSelectedCompletion,
    ))
    .unwrap();
    let ApplicationTransition::PrepareCheckout {
        prompt_id: next_id,
        request: next,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("reselection retries existing preparation")
    };
    assert_eq!(next.id, request.id);
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            prompt_id: next_id,
            result: prepared(&layout, &next),
        })
        .unwrap()
    else {
        panic!("destination binding may be delivered")
    };
    assert_eq!(create.preparation_id, Some(request.id));
    assert_eq!(
        create.prompt.skill_invocations[0].skill_id,
        SkillId::new("destination-review")
    );
}

#[test]
fn server_replacement_restores_draft_and_retries_the_same_worktree_intent_after_edits() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Original draft");
    let ApplicationTransition::PrepareCheckout { request, .. } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    app.handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
        layout.health(uuid::Uuid::new_v4(), 43_424),
    )))
    .unwrap();
    assert!(text(&app).contains("Original draft"));
    type_terminal_text(&mut app, " edited");
    let ApplicationTransition::PrepareCheckout {
        request: retry,
        prompt_id,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("retry is usable after replacement")
    };
    assert_eq!(retry.id, request.id);
    assert_eq!(retry.source, request.source);
    assert!(retry.description.contains("edited"));
    let mut result = prepared(&layout, &retry);
    let intended = result.preparation.intended_session;
    result.preparation.admitted_session = Some(intended);
    result.location = None;
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared { prompt_id, result })
        .unwrap()
    else {
        panic!("admitted retry rejoins through existing creation")
    };
    assert_eq!(create.preparation_id, Some(request.id));
}

#[test]
fn linked_removal_confirmation_warns_counts_cancel_is_read_only_and_force_is_distinct() {
    let layout = Layout::new();
    let mut app = layout.app();
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, layout.context.clone());
    let ApplicationTransition::PreviewCheckoutRemoval { request_id, target } =
        key(&mut app, KeyCode::Char('d'))
    else {
        panic!("read-only preview");
    };
    let preview = CheckoutRemovalPreview {
        target,
        inspection: CheckoutRemovalInspection {
            checkout: layout.context.checkouts[1].clone(),
            tracked: vec!["tracked".into()],
            untracked: vec![],
            ignored: vec!["secret".into()],
            lock: Some("owner lock".into()),
            initialized_submodules: vec!["module".into()],
        },
        affected_sessions: 3,
        working_sessions: 0,
    };
    app.handle_event(ApplicationEvent::CheckoutRemoval {
        request_id,
        result: Ok(RemoveCheckoutResult {
            preview: preview.clone(),
            removed: false,
            error: None,
        }),
    })
    .unwrap();
    let rendered = text(&app);
    assert!(rendered.contains("3 affected Sessions"));
    assert!(rendered.contains("Ignored contents"));
    assert!(rendered.contains("owner lock"));
    assert!(rendered.contains("Initialized submodules"));
    assert!(matches!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::Continue
    ));
    let ApplicationTransition::RemoveCheckout {
        request_id,
        request,
    } = key(&mut app, KeyCode::Char('F'))
    else {
        panic!("explicit force command");
    };
    assert!(request.force);
    assert_eq!(request.preview, preview);
    app.handle_event(ApplicationEvent::CheckoutRemoval {
        request_id,
        result: Ok(RemoveCheckoutResult {
            preview,
            removed: false,
            error: Some("Git refused removal".into()),
        }),
    })
    .unwrap();
    assert!(matches!(
        key(&mut app, KeyCode::Esc),
        ApplicationTransition::Continue
    ));
    assert!(!text(&app).contains("3 affected Sessions"));
    assert!(layout.linked.exists());
}
#[test]
fn main_removal_unavailable_and_canceled_preview_response_is_ignored() {
    let layout = Layout::new();
    let mut app = layout.app();
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, layout.context.clone());
    key(&mut app, KeyCode::Down);
    assert!(matches!(
        key(&mut app, KeyCode::Char('d')),
        ApplicationTransition::Continue
    ));
    assert!(text(&app).contains("Main checkout cannot be removed"));
    key(&mut app, KeyCode::Down);
    let ApplicationTransition::PreviewCheckoutRemoval { request_id, target } =
        key(&mut app, KeyCode::Char('d'))
    else {
        panic!("preview linked");
    };
    key(&mut app, KeyCode::Esc);
    let preview = CheckoutRemovalPreview {
        target,
        inspection: CheckoutRemovalInspection {
            checkout: layout.context.checkouts[1].clone(),
            tracked: vec![],
            untracked: vec![],
            ignored: vec![],
            lock: None,
            initialized_submodules: vec![],
        },
        affected_sessions: 0,
        working_sessions: 0,
    };
    app.handle_event(ApplicationEvent::CheckoutRemoval {
        request_id,
        result: Ok(RemoveCheckoutResult {
            preview,
            removed: false,
            error: None,
        }),
    })
    .unwrap();
    assert!(!text(&app).contains("affected Sessions"));
}

#[test]
fn successful_removal_marks_selected_exact_directory_unavailable_and_keeps_it_selected() {
    let layout = Layout::new();
    let mut app = layout.app();
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, layout.context.clone());
    let ApplicationTransition::PreviewCheckoutRemoval { request_id, target } =
        key(&mut app, KeyCode::Char('d'))
    else {
        panic!("preview linked");
    };
    let preview = CheckoutRemovalPreview {
        target,
        inspection: CheckoutRemovalInspection {
            checkout: layout.context.checkouts[1].clone(),
            tracked: vec![],
            untracked: vec![],
            ignored: vec![],
            lock: None,
            initialized_submodules: vec![],
        },
        affected_sessions: 2,
        working_sessions: 0,
    };
    app.handle_event(ApplicationEvent::CheckoutRemoval {
        request_id,
        result: Ok(RemoveCheckoutResult {
            preview: preview.clone(),
            removed: false,
            error: None,
        }),
    })
    .unwrap();
    let ApplicationTransition::RemoveCheckout {
        request_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("ordinary removal confirmation");
    };
    assert!(!request.force);
    app.handle_event(ApplicationEvent::CheckoutRemoval {
        request_id,
        result: Ok(RemoveCheckoutResult {
            preview,
            removed: true,
            error: None,
        }),
    })
    .unwrap();
    let rendered = text(&app);
    assert!(rendered.contains("unavailable"));
    assert!(rendered.contains("nested"));
    key(&mut app, KeyCode::Esc);
    type_terminal_text(&mut app, "Do not redirect my next Session");
    assert!(matches!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::Continue
    ));
    assert!(text(&app).contains("Execution Directory unavailable"));
}
