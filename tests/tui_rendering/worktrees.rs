//! Landing execution navigation through semantic commands and Server responses.
use crate::support::{
    buffer_rows, click_mouse, deliver_settings, fixture_instance_id, ready_health,
    rendered_application_buffer, rendered_application_rows_at, selector_label, type_terminal_text,
    workspace_resolution,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::path::{Path, PathBuf};
use suru::{
    managed_client::ManagedEvent,
    protocol::*,
    tui::{
        Application, ApplicationEvent, ApplicationTransition, ClipboardRead, CommandId,
        SemanticCommandId,
    },
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
            repository: Some(Box::new(repository)),
            source_control: SourceControlAvailability::Available,
            icon: None,
            description: None,
        };
        let checkouts = [(&main, CheckoutKind::Main), (&linked, CheckoutKind::Linked)]
            .into_iter()
            .map(|(root, kind)| CheckoutSummary {
                association: CheckoutAssociation {
                    recovery_revision: None,
                    reclaim: None,
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
        self.app_in(self.context.clone())
    }
    /// A Landing the owning Server answered `context` for.
    fn app_in(&self, context: ResolvedWorkspace) -> Application {
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
        answer(&mut app, transition, context);
        app
    }
}
fn answer(app: &mut Application, transition: ApplicationTransition, context: ResolvedWorkspace) {
    let (outlook, surface, request_id, _) = workspace_resolution(&transition)
        .unwrap_or_else(|| panic!("expected owning Server resolution: {transition:?}"));
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
fn show_icons(app: &mut Application) {
    let mut settings = EffectiveSettings::default();
    settings.appearance.show_icons = true;
    deliver_settings(app, settings);
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
            approval_posture: None,
            status: SessionStatus::Idle,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        },
        title: title.to_owned(),
        icon: None,
        settled_at: None,
        standing_inputs: Default::default(),
        total_usage: None,
        own_cost: None,
        remote_subsessions: Vec::new(),
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
    assert!(rows.contains("abcdef0"), "{rows}");
    key(&mut app, KeyCode::Down);
    let transition = key(&mut app, KeyCode::Enter);
    let ApplicationTransition::ResolveWorkspace { request, .. } = &transition else {
        panic!("select checkout on Server")
    };
    assert_eq!(
        request.checkout_id,
        Some(layout.context.checkouts[0].association.id.clone())
    );
    assert_eq!(
        request.workspace_id,
        Some(layout.context.workspace.id.clone())
    );
    answer(&mut app, transition, layout.at(&layout.main));
    type_terminal_text(&mut app, "Use this checkout");
    let ApplicationTransition::CreateSession(request) = key(&mut app, KeyCode::Enter) else {
        panic!("start new Session")
    };
    assert_eq!(request.execution_directory.path, layout.main);
}

/// The rows the Worktree Selector offers, in the order it offers them: a new
/// Worktree first and selected, then every Worktree the Repository has, with
/// the one the next Session already stands in marked rather than moved.
#[test]
fn the_selector_leads_with_a_new_worktree_and_marks_the_current_one() {
    let layout = Layout::new();
    let mut app = layout.app();
    open(&mut app, layout.context.clone());
    let rendered = text(&app);
    let rows = rendered
        .lines()
        .skip_while(|row| !row.contains("Current:"))
        .skip(1)
        .take(3)
        .map(|row| row.trim().trim_start_matches('│').trim().to_owned())
        .collect::<Vec<_>>();
    assert!(rows[0].starts_with("› New Worktree"), "{rendered}");
    assert!(rows[1].starts_with("main ·"), "{rendered}");
    assert!(
        rows[2].starts_with("* detached abcdef01 ·"),
        "the Worktree in use is marked: {rendered}"
    );
    assert!(!rows[1].contains('*'), "{rendered}");
    // Enter on the marked row is the reader saying they are staying put, so
    // nothing is asked of the owning Server and the selector closes.
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    assert_eq!(
        key(&mut app, KeyCode::Enter),
        ApplicationTransition::Continue
    );
    assert!(!text(&app).contains(" Worktrees "));
}

/// A Selector row is a Worktree the reader is choosing between rather than one
/// they are standing in, so it says what a branch with no commits behind it is.
#[test]
fn the_selector_says_an_unborn_branch_is_unborn() {
    let layout = Layout::new();
    let mut app = layout.app();
    let mut context = layout.context.clone();
    context.checkouts[0].revision = Some(CheckoutRevision::Branch {
        name: "main".to_owned(),
        commit: None,
    });
    open(&mut app, context);
    let rendered = text(&app);
    assert!(rendered.contains("main (unborn) ·"), "{rendered}");
}

/// A detached head says so in full on a Selector row, with commit enough to
/// tell two of them apart.
#[test]
fn the_selector_says_a_detached_head_is_detached() {
    let layout = Layout::new();
    let mut app = layout.app();
    open(&mut app, layout.context.clone());
    let rendered = text(&app);
    assert!(rendered.contains("detached abcdef01 ·"), "{rendered}");
    // The Landing beneath keeps the Sidebar's shorter reading of the same
    // Worktree: one shared label, two forms.
    key(&mut app, KeyCode::Esc);
    let landing = text(&app);
    assert!(!landing.contains("detached"), "{landing}");
    assert!(landing.contains("abcdef0"), "{landing}");
}

/// The Landing says which Checkout State the next Session begins on, and says
/// it as a Sidebar row does.
#[test]
fn the_landing_names_the_current_checkout_state_and_prefers_the_live_reading() {
    let layout = Layout::new();
    let mut linked = layout.context.clone();
    linked.checkouts[1].revision = Some(CheckoutRevision::Branch {
        name: "feature/landing".to_owned(),
        commit: Some("1234567890abcdef".to_owned()),
    });
    let mut app = layout.app();
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, linked.clone());
    key(&mut app, KeyCode::Esc);
    assert!(
        text(&app).contains("feature/landing (worktree)"),
        "{}",
        text(&app)
    );
    let mut moved = linked.checkouts[1].clone();
    moved.revision = Some(CheckoutRevision::Branch {
        name: "feature/moved".to_owned(),
        commit: Some("1234567890abcdef".to_owned()),
    });
    app.handle_event(ApplicationEvent::Managed(
        ManagedEvent::CheckoutStateChanged(CheckoutStateChanged {
            checkout_id: moved.association.id.clone(),
            checkout_state: Some(moved),
        }),
    ))
    .unwrap();
    let rendered = text(&app);
    assert!(rendered.contains("feature/moved (worktree)"), "{rendered}");
    let location = Path::new("~").join("main");
    assert!(
        rendered.contains(&format!(
            "{} · feature/moved (worktree) · nested",
            location.display()
        )),
        "{rendered}"
    );
    assert!(!rendered.contains('\u{ef81}'), "{rendered}");
    assert!(!rendered.contains("feature/landing"), "{rendered}");
}

#[test]
fn landing_icons_decorate_the_location_checkout_and_pending_worktree() {
    let layout = Layout::new();
    let mut linked = layout.context.clone();
    linked.checkouts[1].revision = Some(CheckoutRevision::Branch {
        name: "feature/landing-icons".to_owned(),
        commit: Some("1234567890abcdef".to_owned()),
    });
    let mut app = layout.app();
    show_icons(&mut app);
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, linked);
    key(&mut app, KeyCode::Esc);

    let rendered = text(&app);
    let location = Path::new("~").join("main");
    assert!(
        rendered.contains(&format!(" {}", location.display())),
        "{rendered}"
    );
    assert!(
        rendered.contains(" feature/landing-icons ·  nested"),
        "{rendered}"
    );
    assert!(!rendered.contains("(worktree)"), "{rendered}");

    choose_new(&mut app, &layout);
    let pending = text(&app);
    assert!(pending.contains(" New Worktree on submit"), "{pending}");
}

/// The Landing draws the current Workspace's own derived Icon in place of the
/// plain folder glyph, falls back to the folder glyph while it has none, and
/// draws neither with Icons off.
#[test]
fn the_landing_draws_the_workspaces_own_icon_in_place_of_the_folder_glyph() {
    let mut layout = Layout::new();

    let plain = layout.app();
    let plain_text = text(&plain);
    assert!(
        !plain_text.contains('\u{ea83}') && !plain_text.contains('\u{e7a8}'),
        "no Icon is drawn while the reader keeps Icons off: {plain_text}"
    );

    let mut folder_app = layout.app();
    show_icons(&mut folder_app);
    let folder_text = text(&folder_app);
    assert!(
        folder_text.contains('\u{ea83}'),
        "the folder glyph stands while the Workspace has no derived Icon: {folder_text}"
    );

    layout.context.workspace.icon = Some("dev-rust".to_owned());
    let mut iconed_app = layout.app();
    show_icons(&mut iconed_app);
    let iconed_text = text(&iconed_app);
    assert!(
        iconed_text.contains('\u{e7a8}') && !iconed_text.contains('\u{ea83}'),
        "the Workspace's own derived Icon replaces the folder glyph: {iconed_text}"
    );
}

#[test]
fn landing_icons_distinguish_main_detached_and_unavailable_checkout_states() {
    let layout = Layout::new();
    let mut app = layout.app();
    show_icons(&mut app);

    let mut main = layout.at(&layout.main);
    main.checkouts[0].revision = Some(CheckoutRevision::Branch {
        name: "main".to_owned(),
        commit: Some("1234567890abcdef".to_owned()),
    });
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, main);
    key(&mut app, KeyCode::Esc);
    assert!(text(&app).contains(" main"), "{}", text(&app));
    assert!(!text(&app).contains('\u{ef81}'), "{}", text(&app));

    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, layout.context.clone());
    key(&mut app, KeyCode::Esc);
    let detached = text(&app);
    assert!(detached.contains(" abcdef0"), "{detached}");
    assert!(!detached.contains("(worktree)"), "{detached}");

    let mut unavailable = layout.context.clone();
    unavailable.checkouts[1].availability = SourceControlAvailability::Unavailable {
        reason: "gone".to_owned(),
    };
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, unavailable);
    key(&mut app, KeyCode::Esc);
    let unavailable = text(&app);
    assert!(unavailable.contains("[unavailable]"), "{unavailable}");
    assert!(!unavailable.contains(''));
    assert!(!unavailable.contains(''));
    assert!(!unavailable.contains(''));
}

#[test]
fn landing_remote_and_path_icons_are_measured_by_existing_truncation() {
    let layout = Layout::new();
    let mut app = layout.app();
    show_icons(&mut app);
    crate::connecting::turn_to_studio(&mut app);
    app.handle_event(ApplicationEvent::WorkspaceResolved {
        outlook: Outlook::Remote("studio".to_owned()),
        surface: suru::tui::WorkspaceResolutionSurface::Outlook,
        request_id: 2,
        result: Ok(layout.context.clone()),
    })
    .unwrap();

    let wide = text(&app);
    assert!(wide.contains("󰍹 studio ·  "), "{wide}");

    let narrow = rendered_application_rows_at(&app, 28, 20)
        .into_iter()
        .find(|row| row.contains("󰍹 studio"))
        .expect("narrow Landing location row");
    assert!(narrow.contains('…'), "{narrow}");
    assert_eq!(
        unicode_width::UnicodeWidthStr::width(narrow.trim()),
        26,
        "the two horizontal padding cells leave 26 measured columns: {narrow}"
    );
}

/// Turning the Outlook is turning toward another Server's world. The Landing
/// answers for the Server it is turned to alone, so a Checkout State the
/// Server it left resolved is left behind with it: until the Remote resolves a
/// context of its own the footer has nothing to say beyond the path.
#[test]
fn turning_to_a_remote_leaves_the_local_checkout_state_behind() {
    let layout = Layout::new();
    let mut local = layout.context.clone();
    local.checkouts[1].revision = Some(CheckoutRevision::Branch {
        name: "local-only".to_owned(),
        commit: Some("1234567890abcdef".to_owned()),
    });
    let mut app = layout.app();
    let transition = command(&mut app, SemanticCommandId::WorktreeList);
    answer(&mut app, transition, local);
    key(&mut app, KeyCode::Esc);
    assert!(text(&app).contains("local-only"), "{}", text(&app));
    crate::connecting::turn_to_studio(&mut app);
    let remote = text(&app);
    assert!(
        !remote.contains("local-only"),
        "the Remote's Landing says nothing of the Local Server's branch: {remote}"
    );
}

/// A Worktree Suru made is known by the name it was given, at whatever depth
/// under `.suru-worktrees` the Server that made it chose to put it: what the
/// managed directory holds beneath itself is Suru's own business rather than
/// the reader's. Both a Worktree standing directly beneath it and one standing
/// a level further down are offered by their leaf name alone.
#[test]
fn managed_worktrees_are_offered_by_their_leaf_name() {
    let layout = Layout::new();
    let mut app = layout.app();
    let mut context = layout.context.clone();
    let repository = context.workspace.repository.clone().unwrap();
    for managed in [
        layout.main.join(".suru-worktrees/review-landing"),
        layout.main.join(".suru-worktrees/nested/review-selector"),
    ] {
        context.checkouts.push(CheckoutSummary {
            association: CheckoutAssociation {
                recovery_revision: None,
                reclaim: None,
                id: CheckoutId::from_root(&repository.id, &managed),
                repository: repository.id.clone(),
                root: managed,
                kind: CheckoutKind::Linked,
            },
            revision: Some(CheckoutRevision::Branch {
                name: "suru/review".to_owned(),
                commit: Some("1234567890abcdef".to_owned()),
            }),
            availability: SourceControlAvailability::Available,
        });
    }
    open(&mut app, context);
    let rendered = text(&app);
    assert!(
        rendered.contains("suru/review (worktree) · review-landing"),
        "{rendered}"
    );
    assert!(
        rendered.contains("suru/review (worktree) · review-selector"),
        "{rendered}"
    );
    assert!(!rendered.contains(".suru-worktrees"), "{rendered}");
}

/// A Workspace whose Server answers with a directory below one of its
/// Worktrees works there, so the Skills on offer are the ones read for that
/// directory. The Worktree Selector offers Worktrees and nothing else.
#[test]
fn a_subdirectory_the_server_resolves_is_where_destination_skills_are_read() {
    let layout = Layout::new();
    let mut app = layout.app();
    let destination = layout.linked.join("another directory");
    std::fs::create_dir(&destination).unwrap();
    let other = ResolvedWorkspace::directory(layout.main.parent().unwrap().join("other"));
    let transition = pick_other_workspace(&mut app, &[layout.context.clone(), other.clone()]);
    answer(&mut app, transition, other.clone());
    // Back to the Repository, whose Server answers with the directory below
    // its linked Worktree.
    let transition = pick_other_workspace(&mut app, &[layout.context.clone(), other]);
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
    let ApplicationTransition::DetachSessionAndResolveWorkspace { request, .. } = &transition
    else {
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

/// The Landing names a Workspace chosen in the picker before its Server has
/// answered for it, and names nothing else of it: the Worktree, Checkout
/// State, and subdirectory it was showing belong to the Workspace being left,
/// and the chosen one's are not yet known. Nor does it offer the Worktrees of
/// a Repository it cannot yet name; once the Server answers, the Landing
/// says what it said.
#[test]
fn a_workspace_still_resolving_is_named_without_the_worktree_left_behind() {
    let layout = Layout::new();
    let mut app = layout.app();
    let left = text(&app);
    assert!(left.contains("abcdef0 · nested"), "{left}");

    let other = ResolvedWorkspace::directory(layout.root.join("other"));
    let transition = pick_other_workspace(&mut app, &[layout.context.clone(), other.clone()]);
    let waiting = text(&app);
    let location = Path::new("~").join("other");
    assert!(
        waiting.contains(&location.display().to_string()),
        "the Landing names the Workspace chosen: {waiting}"
    );
    assert!(
        !waiting.contains("abcdef0") && !waiting.contains("nested"),
        "and nothing of the Worktree left behind as though it were the chosen one's: {waiting}"
    );
    assert_eq!(
        command(&mut app, SemanticCommandId::WorktreeList),
        ApplicationTransition::Continue,
        "no Worktree is offered until the Server says which Repository it would be of"
    );
    assert!(!text(&app).contains("Loading Worktrees"), "{}", text(&app));

    answer(&mut app, transition, other);
    let resolved = text(&app);
    assert!(
        resolved.contains(&location.display().to_string()) && !resolved.contains("nested"),
        "{resolved}"
    );
    assert!(matches!(
        command(&mut app, SemanticCommandId::WorktreeList),
        ApplicationTransition::ResolveWorkspace { .. }
    ));
}

/// A way of framing the Landing that moves where the readings beneath its
/// composer are drawn: the size of the frame, and what is arranged around the
/// Landing before it is drawn.
#[derive(Clone, Copy)]
struct Framing {
    what: &'static str,
    size: (u16, u16),
    arrange: fn(&mut Application),
}

/// The framings every reading beneath the Landing's composer is pressed in:
/// a roomy frame; beside the Sidebar, which moves every column over; beneath
/// a draft of several lines, which moves the line down; and a terminal narrow
/// enough to narrow the padding and, where the line runs long, cut it.
const FRAMINGS: [Framing; 4] = [
    Framing {
        what: "a roomy frame",
        size: (160, 30),
        arrange: |_| {},
    },
    Framing {
        what: "beside the Sidebar",
        size: (160, 30),
        arrange: show_sidebar,
    },
    Framing {
        what: "beneath a draft of several lines",
        size: (160, 30),
        arrange: write_long_draft,
    },
    Framing {
        what: "a narrow terminal",
        size: NARROW,
        arrange: |_| {},
    },
];

/// A terminal narrow enough that the line beneath a pending Worktree intent
/// no longer fits beneath the composer.
const NARROW: (u16, u16) = (40, 30);

fn show_sidebar(app: &mut Application) {
    deliver_settings(
        app,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Shown,
                ..Default::default()
            },
            ..Default::default()
        },
    );
}

fn write_long_draft(app: &mut Application) {
    app.handle_terminal_event(InputEvent::Paste("A draft\nof several\nlines".to_owned()))
        .unwrap();
}

/// A Landing laid out as `framing` arranges it, with `prepare` done to it
/// first.
fn framed(layout: &Layout, framing: Framing, prepare: impl Fn(&mut Application)) -> Application {
    let mut app = layout.app();
    prepare(&mut app);
    (framing.arrange)(&mut app);
    app
}

/// The rows of `app` drawn in a frame of `size`, and the row beneath its
/// composer, where the readings stand.
fn reading_line(app: &Application, size: (u16, u16)) -> (Vec<String>, usize) {
    let rows = buffer_rows(&rendered_application_buffer(app, size.0, size.1));
    let composer_bottom = rows
        .iter()
        .rposition(|row| row.contains("└─"))
        .unwrap_or_else(|| panic!("the view draws its composer: {rows:#?}"));
    (rows, composer_bottom + 1)
}

/// The cell `reading` begins at on the line beneath the composer, drawn in a
/// frame of `size`, found on the frame so a press lands where the reader
/// would point rather than on a column the test assumes.
fn reading_cell(app: &Application, size: (u16, u16), reading: &str) -> (u16, u16) {
    let (rows, line) = reading_line(app, size);
    let offset = rows[line]
        .find(reading)
        .unwrap_or_else(|| panic!("{reading:?} is not beneath the composer: {}", rows[line]));
    (
        rows[line][..offset].chars().count().try_into().unwrap(),
        line.try_into().unwrap(),
    )
}

/// Presses the pointer at `cell` of the frame `app` draws at `size`, the way
/// a reader does: on a frame already on screen.
fn press_at(
    app: &mut Application,
    size: (u16, u16),
    (column, row): (u16, u16),
) -> ApplicationTransition {
    rendered_application_buffer(app, size.0, size.1);
    click_mouse(
        app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .unwrap()
}

fn press_reading(app: &mut Application, size: (u16, u16), reading: &str) -> ApplicationTransition {
    let cell = reading_cell(app, size, reading);
    press_at(app, size, cell)
}

/// Presses `reading` and answers whether the press moved nothing: no
/// transition, and the frame drawn as it was.
fn press_moves_nothing(app: &mut Application, size: (u16, u16), reading: &str) -> bool {
    let before = rendered_application_rows_at(app, size.0, size.1);
    press_reading(app, size, reading) == ApplicationTransition::Continue
        && rendered_application_rows_at(app, size.0, size.1) == before
}

/// The Workspace named beneath the Landing's composer is a way in under the
/// pointer: pressing it opens the Workspace Picker, asking exactly what
/// `/workspace` asks, wherever the frame puts it.
#[test]
fn pressing_the_workspace_beneath_the_landing_composer_opens_the_workspace_picker() {
    let layout = Layout::new();
    let location = Path::new("~").join("main").display().to_string();
    let mut cells = Vec::new();
    for framing in FRAMINGS {
        let asked = command(
            &mut framed(&layout, framing, |_| {}),
            SemanticCommandId::WorkspaceList,
        );
        let mut app = framed(&layout, framing, |_| {});
        cells.push(reading_cell(&app, framing.size, &location));

        let pressed = press_reading(&mut app, framing.size, &location);

        assert!(
            matches!(pressed, ApplicationTransition::ListSessions(_)),
            "{}: {pressed:?}",
            framing.what
        );
        assert_eq!(
            pressed, asked,
            "{}: the press asks what `/workspace` asks",
            framing.what
        );
        let drawn = rendered_application_rows_at(&app, framing.size.0, framing.size.1).join("\n");
        assert!(drawn.contains(" Workspaces "), "{}: {drawn}", framing.what);
    }
    // Each framing draws the line somewhere else, which is what makes it a
    // case of its own.
    for (index, cell) in cells.iter().enumerate() {
        assert!(
            !cells[..index].contains(cell),
            "{} draws the Workspace where another framing does: {cells:?}",
            FRAMINGS[index].what
        );
    }
}

/// The Checkout State beneath the Landing's composer opens the Worktree
/// Selector as `/worktree` does, wherever the frame puts it.
#[test]
fn pressing_the_checkout_state_beneath_the_landing_composer_opens_the_worktree_selector() {
    let layout = Layout::new();
    for framing in FRAMINGS {
        let asked = command(
            &mut framed(&layout, framing, |_| {}),
            SemanticCommandId::WorktreeList,
        );
        let mut app = framed(&layout, framing, |_| {});

        let pressed = press_reading(&mut app, framing.size, "abcdef0");

        assert!(
            matches!(pressed, ApplicationTransition::ResolveWorkspace { .. }),
            "{}: {pressed:?}",
            framing.what
        );
        assert_eq!(
            pressed, asked,
            "{}: the press asks what `/worktree` asks",
            framing.what
        );
        answer(&mut app, pressed, layout.context.clone());
        let drawn = rendered_application_rows_at(&app, framing.size.0, framing.size.1).join("\n");
        assert!(drawn.contains("Current:"), "{}: {drawn}", framing.what);
    }
}

/// A pending "New Worktree on submit" is revised where it is shown: pressing
/// it opens the Worktree Selector as `/worktree` does, wherever the frame puts
/// it — in a narrow terminal, on what the cut line leaves of it.
#[test]
fn pressing_the_pending_worktree_intent_beneath_the_landing_composer_opens_the_worktree_selector() {
    let layout = Layout::new();
    let pending = |app: &mut Application| choose_new(app, &layout);
    for framing in FRAMINGS {
        let asked = command(
            &mut framed(&layout, framing, pending),
            SemanticCommandId::WorktreeList,
        );
        let mut app = framed(&layout, framing, pending);

        let pressed = press_reading(&mut app, framing.size, "New Worktree");

        assert!(
            matches!(pressed, ApplicationTransition::ResolveWorkspace { .. }),
            "{}: {pressed:?}",
            framing.what
        );
        assert_eq!(
            pressed, asked,
            "{}: the press asks what `/worktree` asks",
            framing.what
        );
        let drawn = rendered_application_rows_at(&app, framing.size.0, framing.size.1).join("\n");
        assert!(
            drawn.contains("Loading Worktrees"),
            "{}: {drawn}",
            framing.what
        );
    }
}

/// Icons drawn in front of the readings move where each one stands, and a
/// press still lands on the reading drawn under it: the pending intent stands
/// last, behind every glyph the line draws.
#[test]
fn the_landing_readings_answer_the_pointer_where_icons_are_drawn() {
    let layout = Layout::new();
    let mut twin = layout.app();
    show_icons(&mut twin);
    choose_new(&mut twin, &layout);
    let asked = command(&mut twin, SemanticCommandId::WorktreeList);
    let mut app = layout.app();
    show_icons(&mut app);
    choose_new(&mut app, &layout);

    assert_eq!(
        press_reading(&mut app, (160, 30), "New Worktree on submit"),
        asked
    );
}

/// A line too long for its frame gives up the Workspace's path to the marker
/// saying it was cut, then is cut at its end. A reading the end cut runs
/// through answers over what is left of it, the marker ending the line takes
/// no press, and the Workspace, though only its marker is left, still opens
/// the Workspace Picker.
#[test]
fn a_reading_the_line_cuts_short_answers_over_what_is_left_of_it() {
    let layout = Layout::new();
    let cut = || {
        let mut app = layout.app();
        choose_new(&mut app, &layout);
        app
    };
    let (rows, line) = reading_line(&cut(), NARROW);
    let drawn = rows[line].trim();
    assert!(
        drawn.starts_with('…') && drawn.ends_with('…') && !drawn.contains("on submit"),
        "the narrow line is cut at both ends of its readings: {drawn}"
    );
    let (column, row) = reading_cell(&cut(), NARROW, drawn);
    let last = column + u16::try_from(drawn.chars().count() - 1).unwrap();

    let mut app = cut();
    let before = rendered_application_rows_at(&app, NARROW.0, NARROW.1);
    assert_eq!(
        press_at(&mut app, NARROW, (last, row)),
        ApplicationTransition::Continue
    );
    assert_eq!(
        rendered_application_rows_at(&app, NARROW.0, NARROW.1),
        before
    );

    let mut app = cut();
    let cell = reading_cell(&app, NARROW, "New Worktree");
    let worktree = command(&mut cut(), SemanticCommandId::WorktreeList);
    assert_eq!(press_at(&mut app, NARROW, (last - 1, cell.1)), worktree);

    let mut app = cut();
    let workspace = command(&mut cut(), SemanticCommandId::WorkspaceList);
    assert_eq!(press_at(&mut app, NARROW, (column, row)), workspace);
}

/// On a Remote the Remote's name stands before the Workspace. The Workspace
/// after it still opens the Workspace Picker; the name is not a choice and
/// takes no press.
#[test]
fn on_a_remote_the_workspace_after_the_remotes_name_opens_the_workspace_picker() {
    let layout = Layout::new();
    let on_studio = || {
        let mut app = layout.app();
        crate::connecting::turn_to_studio(&mut app);
        app.handle_event(ApplicationEvent::WorkspaceResolved {
            outlook: Outlook::Remote("studio".to_owned()),
            surface: suru::tui::WorkspaceResolutionSurface::Outlook,
            request_id: 2,
            result: Ok(layout.context.clone()),
        })
        .unwrap();
        app
    };
    let size = (160, 30);
    let named = "studio · ";
    let (column, row) = reading_cell(&on_studio(), size, named);

    let asked = command(&mut on_studio(), SemanticCommandId::WorkspaceList);
    let mut app = on_studio();
    let workspace = column + u16::try_from(named.chars().count()).unwrap();
    assert_eq!(press_at(&mut app, size, (workspace, row)), asked);

    assert!(
        press_moves_nothing(&mut on_studio(), size, "studio"),
        "the Remote's name takes no press"
    );
}

/// The unavailable marker says the Execution Directory cannot be used; it is
/// not a choice, so a press on it does nothing, wherever the frame puts it.
#[test]
fn pressing_the_unavailable_marker_beneath_the_landing_composer_does_nothing() {
    let layout = Layout::new();
    let mut missing = layout.context.clone();
    missing.execution_status = ExecutionDirectoryStatus::Unavailable {
        reason: "Directory was removed".to_owned(),
    };
    for framing in FRAMINGS {
        let mut app = layout.app_in(missing.clone());
        (framing.arrange)(&mut app);
        assert!(
            press_moves_nothing(&mut app, framing.size, "unavailable"),
            "{}",
            framing.what
        );
    }
}

/// A Checkout State that could not be read is marked unavailable where the
/// Checkout State stands; that marker is not a choice, so a press on it does
/// nothing, wherever the frame puts it.
#[test]
fn pressing_an_unavailable_checkout_state_beneath_the_landing_composer_does_nothing() {
    let layout = Layout::new();
    let mut unreadable = layout.context.clone();
    unreadable.checkouts[1].availability = SourceControlAvailability::Unavailable {
        reason: "HEAD could not be read".to_owned(),
    };
    for framing in FRAMINGS {
        let mut app = layout.app_in(unreadable.clone());
        (framing.arrange)(&mut app);
        assert!(
            press_moves_nothing(&mut app, framing.size, "[unavailable]"),
            "{}",
            framing.what
        );
    }
}

/// The Spinner a Workspace still resolving carries says its reading is on its
/// way, which is not a choice, so a press on it does nothing, wherever the
/// frame puts it.
#[test]
fn pressing_a_workspace_resolution_still_in_flight_does_nothing() {
    let layout = Layout::new();
    let other = ResolvedWorkspace::directory(layout.root.join("other"));
    let resolving = |app: &mut Application| {
        pick_other_workspace(app, &[layout.context.clone(), other.clone()]);
    };
    for framing in FRAMINGS {
        let mut app = framed(&layout, framing, resolving);
        assert!(
            press_moves_nothing(&mut app, framing.size, "⠋"),
            "{}",
            framing.what
        );
    }
}

/// The subdirectory beneath the Landing's composer opens the Directory
/// Browser rooted at that subdirectory, its root row focused, asking exactly
/// what `/browse` asks, wherever the frame puts it.
#[test]
fn pressing_the_subdirectory_beneath_the_landing_composer_opens_the_directory_browser_there() {
    let layout = Layout::new();
    for framing in FRAMINGS {
        let asked = command(
            &mut framed(&layout, framing, |_| {}),
            SemanticCommandId::WorkspaceBrowse,
        );
        let mut app = framed(&layout, framing, |_| {});

        let pressed = press_reading(&mut app, framing.size, "nested");

        let ApplicationTransition::ListDirectory {
            outlook, request, ..
        } = &pressed
        else {
            panic!(
                "{}: the press asks for the subdirectory's children, not {pressed:?}",
                framing.what
            );
        };
        assert_eq!(
            (outlook, &request.path),
            (&Outlook::Local, &layout.nested),
            "{}: the root is the subdirectory, on the Outlook's Server",
            framing.what
        );
        assert_eq!(
            pressed, asked,
            "{}: the press asks what `/browse` asks",
            framing.what
        );
        let drawn = rendered_application_rows_at(&app, framing.size.0, framing.size.1).join("\n");
        assert!(
            drawn.contains("› ▾ nested"),
            "{}: the root row stands first, focused: {drawn}",
            framing.what
        );
    }
}

/// At the root of a Worktree the Landing shows no subdirectory, so nothing
/// past the Checkout State is a way in: a press where the subdirectory would
/// stand reaches nothing, wherever the frame puts the line.
#[test]
fn at_the_root_of_a_worktree_no_subdirectory_takes_a_press() {
    let layout = Layout::new();
    let at_root = layout.at(&layout.linked);
    for framing in FRAMINGS {
        let mut app = layout.app_in(at_root.clone());
        (framing.arrange)(&mut app);
        let (rows, line) = reading_line(&app, framing.size);
        assert!(
            !rows[line].contains("nested"),
            "{}: no subdirectory is shown: {}",
            framing.what,
            rows[line]
        );
        let (column, row) = reading_cell(&app, framing.size, "abcdef0");
        let after = column + u16::try_from("abcdef0".chars().count()).unwrap();
        let subdirectory = u16::try_from(" · nested".chars().count()).unwrap();

        for column in after..after + subdirectory {
            let before = rendered_application_rows_at(&app, framing.size.0, framing.size.1);
            assert_eq!(
                press_at(&mut app, framing.size, (column, row)),
                ApplicationTransition::Continue,
                "{}: column {column}",
                framing.what
            );
            assert_eq!(
                rendered_application_rows_at(&app, framing.size.0, framing.size.1),
                before,
                "{}: column {column}",
                framing.what
            );
        }
    }
}

/// The press is the `/browse` command, so an Outlook turned toward a Remote
/// that has stopped answering refuses it exactly as it refuses `/browse`:
/// nothing is asked of the Remote and the Landing says why.
#[test]
fn pressing_the_subdirectory_is_refused_while_the_remote_is_unreachable() {
    let layout = Layout::new();
    let mut app = layout.app();
    crate::connecting::turn_to_studio(&mut app);
    // The Remote spells its paths for the Landing, which is what lets the
    // Landing say where within the Worktree it works.
    app.handle_event(ApplicationEvent::OriginCatalog {
        outlook: Outlook::Remote("studio".to_owned()),
        event: ManagedEvent::SessionCatalogReconciled(SessionCatalogSnapshot {
            workspace_paths: WorkspacePaths::from_home(Some(&layout.root)),
            revision: SessionCatalogRevision::INITIAL,
            session_ids: Vec::new(),
            checkout_states: Vec::new(),
        }),
    })
    .unwrap();
    app.handle_event(ApplicationEvent::WorkspaceResolved {
        outlook: Outlook::Remote("studio".to_owned()),
        surface: suru::tui::WorkspaceResolutionSurface::Outlook,
        request_id: 2,
        result: Ok(layout.context.clone()),
    })
    .unwrap();
    let size = (160, 30);
    reading_cell(&app, size, "nested");
    crate::support::studio_stops_answering(&mut app, 1, std::time::Duration::from_secs(5));

    assert_eq!(
        press_reading(&mut app, size, "nested"),
        ApplicationTransition::Continue,
        "nothing is asked of a Remote that is not answering"
    );

    let drawn = rendered_application_rows_at(&app, size.0, size.1).join("\n");
    assert!(
        drawn.contains("studio is unreachable"),
        "the refusal is said where `/browse`'s is: {drawn}"
    );
    assert!(!drawn.contains("Path:"), "no browser opens: {drawn}");
}

/// The Provisional Session draws the Landing's line word for word, but it is
/// not the Landing: none of its readings takes a press.
#[test]
fn the_provisional_sessions_line_takes_no_press() {
    let layout = Layout::new();
    let location = Path::new("~").join("main").display().to_string();
    let mut app = layout.app();
    type_terminal_text(&mut app, "Begin here");
    let ApplicationTransition::CreateSession(_) = key(&mut app, KeyCode::Enter) else {
        panic!("the Prompt claims a Provisional Session")
    };
    let size = (160, 30);
    assert!(
        text(&app).contains("Begin here"),
        "the Provisional Session stands in the Landing's place"
    );

    for reading in [location.as_str(), "abcdef0", "nested"] {
        assert!(
            press_moves_nothing(&mut app, size, reading),
            "{reading:?} takes no press beneath the Provisional Session"
        );
    }
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
        ways: vec![Way::Direct("10.0.0.8:7777".parse().unwrap())],
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
fn cancelling_worktree_resolution_does_not_adopt_a_late_destination() {
    let layout = Layout::new();
    let mut app = layout.app();
    open(&mut app, layout.context.clone());
    key(&mut app, KeyCode::Down);
    let transition = key(&mut app, KeyCode::Enter);
    assert!(matches!(
        key(&mut app, KeyCode::Esc),
        ApplicationTransition::CancelWorkspaceResolution(_)
    ));
    answer(&mut app, transition, layout.at(&layout.main));
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
    // The first row is the semantic new-Worktree intent, and where an opened
    // selector already stands.
    assert_eq!(key(app, KeyCode::Enter), ApplicationTransition::Continue);
}
fn prepared(layout: &Layout, request: &PrepareCheckoutRequest) -> PrepareCheckoutResult {
    let destination = layout.main.join(".suru-worktrees/prepared");
    std::fs::create_dir_all(&destination).unwrap();
    let mut location = layout.at(&destination);
    let repository = *location.workspace.repository.clone().unwrap();
    location.checkout = Some(CheckoutAssociation {
        recovery_revision: None,
        reclaim: None,
        id: CheckoutId::from_root(&repository.id, &destination),
        repository: repository.id.clone(),
        root: destination.clone(),
        kind: CheckoutKind::Linked,
    });
    location.checkouts.push(CheckoutSummary {
        association: location.checkout.clone().unwrap(),
        revision: Some(CheckoutRevision::Branch {
            name: "suru/prepared".to_owned(),
            commit: Some("abc".to_owned()),
        }),
        availability: SourceControlAvailability::Available,
    });
    PrepareCheckoutResult {
        preparation: PreparedCheckout {
            id: request.id,
            persisted_at: SessionTimestamp::now(),
            source: request.source.clone(),
            repository,
            destination: ExecutionDirectory { path: destination },
            plan: CheckoutPreparationPlan::Git {
                branch: "suru/prepared".to_owned(),
                source_commit: "abc".to_owned(),
                source_branch: Some("main".to_owned()),
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
    // Choosing the Worktree already in use cancels the intention.
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Enter);
    assert!(!text(&app).contains("New Worktree on submit"));
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Prepare and work");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("first submit prepares")
    };
    let creating = text(&app);
    assert!(creating.contains("Creating worktree"), "{creating}");
    assert!(!creating.contains("Creating worktree (0s"), "{creating}");
    assert_eq!(request.source.path, layout.nested);
    let result = prepared(&layout, &request);
    let destination = result.preparation.destination.clone();
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result,
        })
        .unwrap()
    else {
        panic!("unbound Prompt automatically continues")
    };
    assert_eq!(create.preparation_id, Some(request.id));
    assert_eq!(create.execution_directory, destination);
    assert_eq!(create.prompt.id, prompt_id);
    let admitting = text(&app);
    assert!(admitting.contains("Working"), "{admitting}");
    assert!(!admitting.contains("Creating worktree"), "{admitting}");
}

#[test]
fn a_prepared_worktree_stands_in_place_of_the_pending_intent_beneath_the_claim() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Prepare and work");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("first submit prepares")
    };
    assert!(text(&app).contains("New Worktree on submit"));
    let result = prepared(&layout, &request);
    let destination = result.preparation.destination.path.clone();
    let ApplicationTransition::CreateSession(_) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result,
        })
        .unwrap()
    else {
        panic!("a prepared Worktree continues to creation")
    };

    // The Worktree has been made: the claim names where it stands, not what it
    // was going to do.
    let drawn = text(&app);
    assert!(!drawn.contains("New Worktree on submit"), "{drawn}");
    assert!(
        drawn.contains(&*destination.file_name().unwrap().to_string_lossy()),
        "{drawn}"
    );
}

#[test]
fn a_retry_asks_again_for_the_worktree_it_prepared_wherever_the_reader_has_moved() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Prepare and work");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("first submit prepares")
    };
    let result = prepared(&layout, &request);
    let destination = result.preparation.destination.clone();
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result,
        })
        .unwrap()
    else {
        panic!("a prepared Worktree continues to creation")
    };
    app.handle_event(ApplicationEvent::SessionCreationFailed {
        prompt_id: create.prompt.id,
        code: None,
        error: "Provider unavailable".to_owned(),
    })
    .unwrap();

    // The reader moves to the Worktree already in use, then retries the refused
    // Prompt from the claim it is standing in.
    open(&mut app, layout.at(&layout.main));
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Enter);
    let ApplicationTransition::CreateSession(retry) = key(&mut app, KeyCode::Enter) else {
        panic!("a retry must ask for the Session, never prepare a second Worktree")
    };
    assert_eq!(retry.preparation_id, Some(request.id));
    assert_eq!(retry.execution_directory, destination);
    assert_eq!(retry.prompt.id, create.prompt.id);
}

#[test]
fn preparation_failure_preserves_draft_and_id_and_late_results_cannot_replace_deliberate_choice() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Retry this draft");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    let mut result = prepared(&layout, &request);
    result.error = Some("Destination Skill discovery failed; retry".to_owned());
    result.preparation.ready = false;
    app.handle_event(ApplicationEvent::CheckoutPrepared {
        attempt_id,
        prompt_id,
        result,
    })
    .unwrap();
    let failed = text(&app);
    assert_eq!(failed.matches("Retry this draft").count(), 2, "{failed}");
    assert!(failed.contains("Could not create Session"), "{failed}");
    assert!(
        failed.contains("Destination Skill discovery failed"),
        "{failed}"
    );
    assert!(!failed.contains("Creating worktree"), "{failed}");
    type_terminal_text(&mut app, " corrected");
    let ApplicationTransition::PrepareCheckout {
        attempt_id: retry_attempt,
        prompt_id: retry_id,
        request: retry,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("retry")
    };
    assert_eq!(retry.id, request.id);
    assert_ne!(retry_id, prompt_id);
    open(&mut app, layout.at(&layout.main));
    key(&mut app, KeyCode::Down); // The Worktree in use, which cancels the intention.
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id: retry_attempt,
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

#[test]
fn interrupting_preparation_restores_the_prompt_and_a_retry_ignores_the_late_result() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Original preparation prompt");
    let ApplicationTransition::PrepareCheckout {
        attempt_id: interrupted_attempt,
        prompt_id: interrupted_prompt,
        request: interrupted,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };

    assert_eq!(key(&mut app, KeyCode::Esc), ApplicationTransition::Continue);
    assert_eq!(key(&mut app, KeyCode::Esc), ApplicationTransition::Continue);
    let restored = text(&app);
    assert!(
        restored.contains("Original preparation prompt"),
        "{restored}"
    );
    assert!(!restored.contains("Creating worktree"), "{restored}");

    app.handle_event(ApplicationEvent::Command(CommandId::SelectAll))
        .unwrap();
    app.handle_event(ApplicationEvent::Command(CommandId::InsertText(
        "Edited retry prompt".to_owned(),
    )))
    .unwrap();
    let ApplicationTransition::PrepareCheckout {
        attempt_id: retry_attempt,
        prompt_id: retry_prompt,
        request: retry,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("the interrupted Worktree remains reusable")
    };
    assert_eq!(retry.id, interrupted.id);
    assert_ne!(retry_prompt, interrupted_prompt);

    assert_eq!(
        app.handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id: interrupted_attempt,
            prompt_id: interrupted_prompt,
            result: prepared(&layout, &interrupted),
        })
        .unwrap(),
        ApplicationTransition::Continue,
        "the old attempt cannot admit its first Prompt"
    );
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id: retry_attempt,
            prompt_id: retry_prompt,
            result: prepared(&layout, &retry),
        })
        .unwrap()
    else {
        panic!("the current retry may continue to admission")
    };
    assert_eq!(create.prompt.text, "Edited retry prompt");
    assert_eq!(create.preparation_id, Some(interrupted.id));
}

#[test]
fn unchanged_retry_does_not_let_the_interrupted_attempt_admit_its_prompt() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Retry unchanged");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Esc);
    let ApplicationTransition::PrepareCheckout {
        attempt_id: _retry_attempt,
        prompt_id: retry_prompt_id,
        request: retry,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("retry unchanged")
    };
    assert_eq!(
        retry_prompt_id, prompt_id,
        "unchanged retry keeps Prompt identity"
    );
    assert_eq!(retry.id, request.id);

    assert_eq!(
        app.handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result: prepared(&layout, &request),
        })
        .unwrap(),
        ApplicationTransition::Continue,
        "the interrupted transport attempt cannot admit the unchanged retry"
    );
    assert!(text(&app).contains("Creating worktree"));
}

#[test]
fn interrupted_preparation_failure_cannot_refuse_an_unchanged_retry() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Retry after stale failure");
    let ApplicationTransition::PrepareCheckout {
        attempt_id: interrupted_attempt,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    key(&mut app, KeyCode::Esc);
    key(&mut app, KeyCode::Esc);
    let ApplicationTransition::PrepareCheckout {
        attempt_id: retry_attempt,
        prompt_id: retry_prompt_id,
        request: retry,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("retry unchanged")
    };
    assert_eq!(retry_prompt_id, prompt_id);
    assert_eq!(retry.id, request.id);

    assert_eq!(
        app.handle_event(ApplicationEvent::CheckoutPreparationFailed {
            attempt_id: interrupted_attempt,
            prompt_id,
            error: "old transport failure".to_owned(),
        })
        .unwrap(),
        ApplicationTransition::Continue
    );
    let still_preparing = text(&app);
    assert!(
        still_preparing.contains("Creating worktree"),
        "{still_preparing}"
    );
    assert!(
        !still_preparing.contains("old transport failure"),
        "{still_preparing}"
    );

    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id: retry_attempt,
            prompt_id: retry_prompt_id,
            result: prepared(&layout, &retry),
        })
        .unwrap()
    else {
        panic!("the current retry may continue to admission")
    };
    assert_eq!(create.prompt.text, "Retry after stale failure");
}

#[test]
fn current_preparation_transport_failure_stays_in_the_provisional_retry_view() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Show current failure");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        ..
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    app.handle_event(ApplicationEvent::CheckoutPreparationFailed {
        attempt_id,
        prompt_id,
        error: "current transport failure".to_owned(),
    })
    .unwrap();

    let failed = text(&app);
    assert_eq!(
        failed.matches("Show current failure").count(),
        2,
        "{failed}"
    );
    assert!(failed.contains("Could not create Session"), "{failed}");
    assert!(failed.contains("current transport failure"), "{failed}");
    assert!(!failed.contains("Creating worktree"), "{failed}");
}

#[test]
fn navigating_away_does_not_cancel_background_preparation_or_restore_its_route() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Finish in the background");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    assert_eq!(
        command(&mut app, SemanticCommandId::SessionNew),
        ApplicationTransition::DetachSession
    );
    assert!(!text(&app).contains("Finish in the background"));

    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result: prepared(&layout, &request),
        })
        .unwrap()
    else {
        panic!("ordinary navigation leaves background creation running")
    };
    assert_eq!(create.prompt.text, "Finish in the background");
    assert!(!text(&app).contains("Finish in the background"));
}

#[test]
fn leaving_failed_preparation_restores_its_prompt_and_reuses_the_worktree_intent() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    type_terminal_text(&mut app, "Restore this failed preparation");
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("prepare")
    };
    let mut result = prepared(&layout, &request);
    result.preparation.ready = false;
    result.error = Some("Destination Skills unavailable".to_owned());
    app.handle_event(ApplicationEvent::CheckoutPrepared {
        attempt_id,
        prompt_id,
        result,
    })
    .unwrap();
    command(&mut app, SemanticCommandId::SessionNew);
    assert!(text(&app).contains("Restore this failed preparation"));
    let ApplicationTransition::PrepareCheckout { request: retry, .. } =
        key(&mut app, KeyCode::Enter)
    else {
        panic!("the restored failure remains retryable")
    };
    assert_eq!(retry.id, request.id);
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
fn managed_preparation_carries_explicit_skill_names_directly_to_destination_admission() {
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
    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("source bindings may prepare")
    };
    let result = prepared(&layout, &request);
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result,
        })
        .unwrap()
    else {
        panic!("destination matching is automatic at the Server boundary")
    };
    assert_eq!(create.preparation_id, Some(request.id));
    assert_eq!(
        create.prompt.skill_invocations[0].skill_id,
        SkillId::new("source-review"),
        "the client preserves the request; the Server owns destination rebinding"
    );
}

#[test]
fn managed_preparation_carries_attachment_bindings_for_the_server_to_name_the_worktree_without() {
    let layout = Layout::new();
    let mut app = layout.app();
    choose_new(&mut app, &layout);
    let ApplicationTransition::ReadClipboard(paste) = app
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ComposerClipboardPaste,
        )))
        .unwrap()
    else {
        panic!("a paste reads the clipboard")
    };
    let ApplicationTransition::UploadAttachment { paste, .. } = app
        .handle_event(ApplicationEvent::ClipboardRead {
            paste,
            read: ClipboardRead::Image {
                png: b"\x89PNG\r\n\x1a\nflicker".to_vec(),
            },
        })
        .unwrap()
    else {
        panic!("a clipboard image is uploaded")
    };
    app.handle_event(ApplicationEvent::AttachmentUploaded {
        paste,
        descriptor: AttachmentDescriptor {
            id: AttachmentId::new("flicker-hash"),
            kind: AttachmentKind::Image {
                width: 16,
                height: 16,
            },
            mime_type: "image/png".to_owned(),
            byte_length: 16,
        },
    })
    .unwrap();
    type_terminal_text(&mut app, "fix flicker");

    let ApplicationTransition::PrepareCheckout {
        attempt_id,
        prompt_id,
        request,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("a Prompt with an image prepares its Worktree")
    };
    let bindings = vec![AttachmentBinding {
        attachment_id: AttachmentId::new("flicker-hash"),
        label: "[Image 1]".to_owned(),
        span: TextSpan { start: 0, end: 9 },
    }];
    assert_eq!(request.prompt.text, "[Image 1] fix flicker");
    assert_eq!(
        request.prompt.attachments, bindings,
        "the Server is told which text is a label, so the Worktree is never named from it"
    );

    let result = prepared(&layout, &request);
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result,
        })
        .unwrap()
    else {
        panic!("the prepared Worktree admits the Prompt")
    };
    assert_eq!(create.prompt.attachments, bindings);
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
        attempt_id,
        prompt_id,
    } = key(&mut app, KeyCode::Enter)
    else {
        panic!("retry is usable after replacement")
    };
    assert_eq!(retry.id, request.id);
    assert_eq!(retry.source, request.source);
    assert!(retry.prompt.text.contains("edited"));
    let mut result = prepared(&layout, &retry);
    let intended = result.preparation.intended_session;
    result.preparation.admitted_session = Some(intended);
    result.location = None;
    let ApplicationTransition::CreateSession(create) = app
        .handle_event(ApplicationEvent::CheckoutPrepared {
            attempt_id,
            prompt_id,
            result,
        })
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
    // Walk past the New Worktree row and the main Worktree onto the linked one.
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    let ApplicationTransition::PreviewCheckoutRemoval { request_id, target } =
        key(&mut app, KeyCode::Char('d'))
    else {
        panic!("read-only preview");
    };
    let preview = CheckoutRemovalPreview {
        target: *target,
        inspection: CheckoutRemovalInspection {
            checkout: layout.context.checkouts[1].clone(),
            tracked: vec!["tracked".into()],
            untracked: vec![],
            ignored: vec!["secret".into()],
            lock: Some("owner lock".into()),
            initialized_submodules: vec!["module".into()],
        },
        branch_outcome: CheckoutBranchOutcome::Retained,
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
    assert!(rendered.contains("Branch retained"), "{rendered}");
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
        target: *target,
        inspection: CheckoutRemovalInspection {
            checkout: layout.context.checkouts[1].clone(),
            tracked: vec![],
            untracked: vec![],
            ignored: vec![],
            lock: None,
            initialized_submodules: vec![],
        },
        branch_outcome: CheckoutBranchOutcome::Deleted,
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
    key(&mut app, KeyCode::Down);
    key(&mut app, KeyCode::Down);
    let ApplicationTransition::PreviewCheckoutRemoval { request_id, target } =
        key(&mut app, KeyCode::Char('d'))
    else {
        panic!("preview linked");
    };
    let preview = CheckoutRemovalPreview {
        target: *target,
        inspection: CheckoutRemovalInspection {
            checkout: layout.context.checkouts[1].clone(),
            tracked: vec![],
            untracked: vec![],
            ignored: vec![],
            lock: None,
            initialized_submodules: vec![],
        },
        branch_outcome: CheckoutBranchOutcome::Deleted,
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
    assert!(
        text(&app).contains("Branch will be deleted"),
        "{}",
        text(&app)
    );
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
    assert!(rendered.contains("Branch deleted"), "{rendered}");
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
