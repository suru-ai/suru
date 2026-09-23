# Git awareness and Worktrees

Confirmed design expanding [#169](https://github.com/jake-tucker/suru/issues/169), published as [spec #302](https://github.com/jake-tucker/suru/issues/302) with the `ready-for-agent` label. The [local spec](git-worktrees-spec.md) contains the implementation and testing contract synthesized from this design. Implementation has not begun.

## Agreed

- Design detection, grouping, selection, creation, and removal of Git Worktrees together, then divide implementation into smaller issues.
- Group the main working copy, linked Worktrees, and their subdirectories under the main Repository Workspace. Include externally created Worktrees and already stored Sessions.
- Preserve each Session's exact Execution Directory when grouping it, including subdirectory launches. Agent execution and Skill discovery continue to use that directory.
- Separate clones and nested Repositories remain separate Workspaces. Directories outside source control retain directory-based Workspaces.
- Git is the only source control implementation in scope. Keep integration boundaries open to future systems such as Mercurial and Jujutsu, with operations represented as optional capabilities where appropriate.
- Worktrees are shared places, not owned by individual Sessions. A new Session may use the current Worktree, another existing one, or a newly created one; isolation is explicit.
- A Session's Execution Directory is fixed after its first Turn. Working in another directory starts another Session; external branch changes within the same directory remain possible.
- Settling a Session never removes a Worktree. Deleting the last Session that references a Managed Worktree prompts a separate Reclaim check by the Server under [ADR 0027](adr/0027-reclaim-idle-managed-worktrees-automatically.md) and [Reclaim spec #347](https://github.com/jake-tucker/suru/issues/347); explicit removal remains a separate user action.
- Sidebar rows show live Checkout State, so Sessions sharing a Worktree show the same current branch, with a main/linked Worktree indicator. Detached Worktrees show a short commit ID and unavailable Worktrees are marked explicitly. Historical branch tracking is outside this scope.
- Directories outside supported source control leave the Sidebar's checkout line blank. A known Repository whose state cannot be read is shown as unavailable rather than mistaken for a directory without source control.
- Workspace selection restores the last Execution Directory per Client, Server, and Workspace, defaulting to the main Worktree when none is remembered. Launching Suru preserves the launch directory. A separate Worktree Selector on the Landing chooses another checkout at its root; subdirectory selection remains explicit.
- New-Worktree creation is fully Suru-managed. The user selects that intent on the Landing; Suru generates a branch name with a `suru` prefix, always uses the source checkout's current commit, and manages the destination. There is no branch/base/path form or override. Dependency installation and setup automation are outside this scope.
- Create on first Prompt submission, capturing the source checkout's current commit then. Toggling the Landing choice creates nothing. Prepare the Worktree and Session before starting the Agent; creation failure preserves the draft and reports the problem.
- Generate `suru/<name>`, deriving the name locally from the first Prompt on the owning Server: the preparation request carries the Prompt's text and Skill Invocations, bound Skill markers are removed by their recorded spans (unbound `$tokens` stay as text), and one pure name-shaping function in the source-control layer keeps up to five meaningful ASCII words within 32 characters, dropping English filler words and whole words rather than cutting one, falling back to `work`. The same name is the location beneath the managed container. A taken name — by a location, a branch Git would refuse beside it (compared case-insensitively, directory conflicts included), or another unfinished preparation's plan — is tried again as `-2`, `-3`, … within the bounded attempts; nothing that holds a name is reset or displaced. No AI request stands in front of creation. Once the Session is admitted from that fresh preparation, its Title Errand alone also asks for 2–5 plain words naming the work, and the owning Server renames the branch once to that proposal (prefix stripped, shaped and numbered the same way) through a capability-gated source-control operation, under the preparation serial and the Repository's mutation guard, only while the Worktree is still on the branch Suru created, that branch has no upstream configured, and no retained preparation intent still names it. The rename is recorded as every sharing Session's Checkout State before those locks are released, so recovery uses the new branch at once. Git carries the branch's reflog and configuration, including Reclaim's recorded base, with it; the location keeps its first name. The rename never delays the first Turn, and a derivation that is off, fails, or proposes nothing usable leaves the first name standing.
- A Session in a newly created Worktree executes at its root, even when the source Execution Directory was a subdirectory. Without a Worktree change, the launch directory remains preserved.
- Removing a Worktree does not require deleting its Sessions. The owning Server blocks removal while any of its associated Sessions is Working, including surviving Subagents; otherwise confirm with its affected Session count, retain histories, report whether its branch is retained or deleted under [ADR 0027](adr/0027-reclaim-idle-managed-worktrees-automatically.md) and [Reclaim spec #347](https://github.com/jake-tucker/suru/issues/347), and mark the execution location unavailable. This check follows the existing Server/Origin authority: it does not claim knowledge of another Channel's Sessions or external tools.
- Warn when Git refuses removal and offer an explicit force option, including for tracked changes, untracked files, locks, and initialized submodules where Git supports forcing. Disclose ignored contents before confirmation because Git may delete them even without force. Force does not override Suru's Working-Session check; the main checkout cannot be removed through Git's Worktree removal operation.
- Recreate any known missing linked Worktree when an associated Session is prompted, including externally created Worktrees, at its original path. Use its last-known branch at that branch's current tip, or its last-known commit when detached. Remember the latest observed branch/commit for recovery; this is not a history of Session branches.
- If the recovery branch was deleted, is checked out elsewhere, or the destination belongs to something else, report a clear error and prevent Agent startup. Failed recreation never redirects the Session to another Execution Directory or starts the Provider anyway.
- Support linked Worktrees backed by bare Repositories. Group them under the bare root and require a working-copy selection for execution; never run the Agent at the bare root.
- After creating a Worktree, refresh its Skill Catalog and automatically resolve explicitly selected Skills by their canonical names in the destination, using destination identities before delivery. Matching Skills require no user intervention. Match names case-insensitively using the existing Skill-name rules and require exactly one destination match for every requested Skill. If any name is missing or ambiguous, block the whole Prompt before admission, keep the Provisional Session and retained Worktree, and restore the Prompt for correction with an error naming the affected Skills. Never omit a requested Skill or choose an ambiguous match arbitrarily. An explicit binding selected from the destination Catalog resolves an ambiguous name through ordinary validation. Prompts without explicit Skill Invocations proceed normally.
- Do not initialize or update submodules during creation or recreation. Dependency installation, project setup commands, and configurable actions on Worktree creation remain out of scope.
- Where execution is possible, ordinary Sessions remain available when Git is unavailable or a repository has no commits. Disable new-Worktree creation with the specific reason when no usable source checkout/commit is available, including a bare Workspace without a selected working copy.
- Keep a missing remembered Execution Directory visibly selected until it is recovered or explicitly changed. Never silently substitute another execution location.
- Group by shared Repository identity even when its main checkout cannot be located. Present the main root when known; otherwise present the repository metadata location with “main checkout unknown.” Discovering the main checkout updates presentation without splitting or relocating Sessions.
- Create managed Worktrees at `<main-root>/.suru-worktrees/<name>/`, keeping them on the repository's disk. Anchor this to the main root even when the source checkout is linked; never create another container inside an arbitrary linked Worktree or fall back to Suru's general data directory.
- Add `/.suru-worktrees/` to `$GIT_COMMON_DIR/info/exclude`, preserving existing entries, rather than relying on a container `.gitignore` or changing tracked files. Ordinary cleanup cannot delete this ignore rule; double-force cleanup can still delete nested Worktrees.
- For bare Repositories, use `<bare-root>/.suru-worktrees/<name>/`. If a non-bare Repository's main checkout is unknown, existing Sessions and Worktree selection remain available, but managed creation requires locating the main checkout first.
- Refresh visible Checkout State automatically within roughly two seconds, sharing an observation across Sessions in the same Worktree. Keep this as built-in behavior initially, without new Settings.

The identity decision is recorded in [ADR-0023](adr/0023-separate-workspace-grouping-from-session-execution.md).

## Existing behavior to preserve

- Resolve paths on the owning Server; equal paths on different Servers do not identify the same Workspace.
- Canonicalize readable paths so alternative spellings of one directory agree. Existing startup tolerates a directory it cannot canonicalize; explicit path selection rejects missing directories.
- Workspace labels abbreviate the owning Server user's known home directory as `~` and preserve that machine's separators. Paths outside that home, or with unknown home, retain full labels.
- Listings do not require Session history hydration. Regrouping stored Sessions must preserve execution paths and Provider Resume State.
- If a missing legacy Session path has no surviving evidence of Repository membership, retain its unresolved location rather than inventing a grouping. Resolve membership when evidence becomes available.
- The Workspace Picker lists known Session Workspaces plus the current Workspace, with current first and the rest ordered by recent work, and searches by name. It offers neither an all-Workspaces row nor an add action. Its selection leaves Sidebar scope unchanged; the Sidebar's own path entry narrows scope as it switches.
- Skill discovery and relative-path resolution use the selected Execution Directory rather than the repository grouping root.

## Source control boundary

Keep source control integration separate from the Agent Provider interface: Codex, Copilot, and Claude all consume the same resolved Execution Directory. Repository discovery, checkout observation, and supported working-copy operations belong behind typed source control interfaces, with capabilities controlling which actions the Client can offer. Git is the first implementation; Mercurial, Jujutsu, a plugin loader, and a public plugin API are deferred. The shared model must not require every future source control system to have a Git branch or Worktree.

For Git, resolve the nearest repository first, then use its canonical common metadata directory to recognize related Worktrees. The owning Server performs all discovery, observation, creation, recovery, and removal, including for Remotes. Keep identity separate from presentation paths, and distinguish an unsupported operation, a repository that is unavailable, and a directory outside source control. Named UI actions use semantic command IDs.

## Preparation and retry invariants

- Interrupting during Worktree preparation prevents first-Prompt admission and restores its text to the composer. Let an ongoing Git operation finish safely and retain its checkout for reuse; do not start the Agent for that submission.

- First Prompt submission immediately opens a Provisional Session with the Prompt visible. Show **Creating worktree** without elapsed time throughout checkout creation and destination Skill discovery, then use the ordinary Session-start indicator. Preparation failure stays in that view with a readable error; empty Enter retries the same Prompt and submitted new text replaces it. Retry reuses any retained Worktree. Leaving the failed view preserves its text as the Landing draft.

- Resolve the source commit once for a new preparation. Retrying that preparation keeps its branch, destination, and commit even if the source checkout subsequently moves.
- Reuse a Worktree already created for a submission after draft edits, Skill reselection, preparation failure, or interrupted delivery. Worktree preparation therefore needs an identity independent of Prompt identity and recoverable progress; the implementation must not allocate another checkout merely because a Prompt ID changed.
- Retain an already-created Worktree after a later preparation failure and expose it for reuse or explicit removal. Preparation failure before Prompt admission preserves the draft; after admission, existing failed-Turn semantics apply.
- Recheck filesystem and Git state before mutations or Provider startup. Serialize conflicting Suru operations on the same Repository, while handling external changes through Git's errors and explicit state checks. Force removal cannot bypass the Server's Working-Session check.
- Recovery must restore the actual Execution Directory, including a preserved subdirectory. Restoring a Worktree root is insufficient if that subdirectory remains unavailable; do not redirect the Session to the root.
- Never overwrite a destination that belongs to another checkout, recursively delete an unrelated directory, or reset an existing branch during creation/recovery. Automatic recovery does not override Git locks. Manage only the required stale registration rather than using recovery as a reason to clean up unrelated Worktrees.
- Newly created Worktrees contain the chosen commit without initialized submodules. Source-checkout uncommitted changes and ignored/local files are not copied.

## Proposed delivery slices

These are local planning slices, not published GitHub subissues.

| Slice | Result | Depends on |
| --- | --- | --- |
| 1. Repository and execution identity | Typed source control seam, Git discovery, durable Workspace/Worktree associations, existing-Session regrouping with execution paths preserved | — |
| 2. Existing Worktree navigation and live state | Grouped Workspace lists, Landing selection, current branch/detached/unavailable Sidebar labels, shared observation on the owning Server | 1 |
| 3. Managed first-Prompt preparation | Repository-local storage, generated branch, captured commit, retryable creation without submodule initialization, automatic destination Skill matching, all three Agent Providers | 1, 2 |
| 4. Missing Worktree recovery | Durable latest-known checkout state, original-path recovery, branch/detached handling, destination validation, pre-start failure behavior | 1, 3 |
| 5. Explicit Worktree removal | Associated-Session count and Working guard, content warnings, force option, safe branch outcome, retained history, recovery after later prompting | 2, 3, 4 |

## Acceptance checks

- Launching from a main checkout, linked Worktree, or their subdirectories produces one Workspace grouping while preserving each existing Session's execution path, Skills, Resume State, and history. Same-remote clones, nested repositories, and different Origins remain separate.
- Discover externally created, detached, unborn, bare-backed, and separately located-metadata layouts. Unknown main-root presentation must not split identity when the root is later discovered. Git absence disables only operations that require it.
- Workspace selection restores the per-Client/Server/Workspace execution location; Worktree selection and new creation start at the selected checkout root. Missing remembered locations remain explicit, and bare roots never become execution directories.
- A new-Worktree choice performs no mutation until submission. Creation uses the source commit at submission, a valid `suru/<name>` branch numbered only on collision, and the accepted repository-local/channel path; initiating from a linked checkout still anchors storage at the main root.
- Preserve existing local exclude rules. The main checkout stays clean and the linked checkout sees its own tracked/untracked files. Cover spaces, non-ASCII names, long names, and platform-correct paths without assuming POSIX roots.
- Duplicate/retried preparation, changed drafts, stale Skills, preparation failures, and a restart after filesystem creation must reuse or recover the same preparation without overwriting unrelated state or creating duplicate Worktrees.
- No Provider starts before the destination is ready and its explicit Skills are valid. Verify Codex, Copilot, and Claude startup/resume receive the exact Execution Directory.
- All Sessions sharing a Worktree show current checkout state within the observation interval, including external branch changes, detached state, missing paths, and Remote failures. Share observations and keep listings independent of history hydration.
- Recreate a missing linked Worktree from the retained branch's current tip, or a remembered detached commit, including external paths and after explicit removal. Deleted/occupied branches, locks, missing repository data, replacement directories, and missing execution subdirectories must fail clearly before Agent startup.
- Removal blocks while an associated Session or surviving Subagent on the owning Server is Working. Idle references survive removal; the branch remains. Warn for dirty/untracked/ignored contents, locks, and initialized submodules, and require the selected force action where appropriate. The main checkout cannot be removed through this action.
- All implementation and tests must support Windows, macOS, and Linux. Use temporary Git fixtures rooted per platform and injectable millisecond-scale timings; run implementation tests with `cargo nextest run`. The exploratory checks below ran on Linux and are not cross-platform certification.

## T3 reference findings

Inspected vendored T3 revision `8b2838e0e` under `references/t3code`.

- T3 separates project `workspaceRoot` from thread `worktreePath`; runtime execution prefers the latter (`apps/server/src/orchestration/Layers/ProviderCommandReactor.ts`).
- Startup creates/finds projects by launch directory (`apps/server/src/serverRuntimeStartup.ts`). Its logical project grouping uses normalized remote identity and can combine separate clones (`packages/client-runtime/src/state/projectGrouping.ts`, `apps/server/src/project/RepositoryIdentityResolver.ts`). Neither behavior directly supplies Suru's chosen Workspace identity.
- Git core resolves the common Git directory separately from the working-copy root (`apps/server/src/vcs/GitVcsDriverCore.ts`). Its branch-to-path worktree discovery omits detached Worktrees, so Suru needs to model Worktrees independently of branches.
- T3 can reuse an existing checkout when a selected branch is already checked out, and creates Worktrees during first-turn bootstrap. Its sidebar shows a saved thread branch and separately detects checkout mismatch (`apps/web/src/components/BranchToolbar.logic.ts`, `apps/server/src/ws.ts`, `apps/web/src/components/Sidebar.tsx`). These are reference behaviors, not settled Suru decisions.
- T3 checks other thread references during cleanup and can recreate a missing Worktree before execution. Suru has chosen explicit removal independent of Session deletion, retained Session histories, and recreation when a missing Worktree's Session is prompted.
- T3 initially generates `t3code/<8 lowercase hex characters>` (`packages/shared/src/git.ts`) and can later rename it to `t3code/<AI-generated slug>` from the first Prompt (`apps/server/src/orchestration/Layers/ProviderCommandReactor.ts`). Its destination remains at the original path after branch renaming.
- T3 creates on first send, with a selected base branch and optional fetched remote state (`apps/web/src/components/ChatView.tsx`, `apps/server/src/ws.ts`). Suru's agreed current-checkout-commit rule differs from this.
- T3 recovery uses the saved branch's current tip at the saved Worktree path, including external paths. It skips detached Worktrees, does not validate a replacement directory that already exists, and logs recovery errors before allowing Provider execution to continue (`apps/server/src/orchestration/Layers/ProviderCommandReactor.ts`, `ensureThreadWorktree`). Suru deliberately also supports detached recovery, validates destination identity, and blocks Agent startup on recovery failure.
- T3 recovery runs repository-wide `git worktree prune` before adding the saved path. Locks can prevent that recovery; pruning and unrelated registrations need deliberate handling.
- T3 creation can leave Worktree/branch artifacts when a later bootstrap step fails: it removes the new thread but does not remove those filesystem artifacts. Creation/recovery helpers do not provide a shared repository mutation lock or durable preparation identity.
- Creation can launch a project setup command without waiting for it, while recovery does not rerun that command. Both paths attempt submodule initialization, and neither copies ignored/local configuration files. Suru excludes dependency installation, setup automation, and automatic submodule initialization.

## Suru first-Prompt integration findings

- Every supported Provider's Skill IDs include the Execution Directory, even for globally installed Skills (`src/provider/{codex,copilot,claude}/skills.rs`). Discovery requires the directory to exist (`src/skill_catalog.rs`), so a source-checkout Skill binding cannot simply be sent in a newly created Worktree.
- General stale-Skill handling retains the draft and requires editing or choosing the Skill again. New-Worktree preparation is an explicit exception: resolve requested canonical names automatically in the destination Catalog (ADR-0014).
- Editing text or Skill bindings changes Prompt identity. If Worktree preparation has already succeeded, corrected submissions must be able to reuse that same prepared Worktree independently of Prompt identity.
- Existing Session creation validates Skills before admission and again before native delivery. Once a Session/first Prompt is admitted, Provider startup failures produce a failed Turn rather than undoing the Session (`src/server.rs`, `src/provider/orchestration.rs`). The preparation boundary must fit those semantics.

## Git layout and removal findings

Checked with Git 2.55.0, local Git documentation, and disposable repositories; existing repositories were untouched.

- Ordinary and linked checkouts share an absolute common Git directory while each has its own top-level directory. Bare Repositories have no main working copy.
- With `--separate-git-dir`, the common metadata directory and actual main checkout can be unrelated paths; the first `worktree list` record may name the metadata directory instead of the actual main checkout. Neither the common-directory parent nor the first list row universally identifies the main working copy.
- Detached Worktrees have a commit without a branch; unborn Worktrees have a branch name without a commit. Worktree identity must not depend on having an existing branch ref.
- Unforced removal rejects tracked modifications, untracked files, locked Worktrees, initialized submodules, and the main checkout. It can delete ignored contents, and successful removal retains the branch ref.

## Repository-local storage validation

The accepted layout is `<main-root>/.suru-worktrees/<name>/` (or the bare-root equivalent), with `/.suru-worktrees/` in Git's local `info/exclude`. The exploration also tested a container `.gitignore` containing `*`, which established that nested Worktrees work but exposed the ignore-file cleanup problem.

The final accepted variant was separately validated on Linux with Git 2.55.0: normal and bare-root nested Worktree creation succeeded; appending the local exclude preserved existing content, including a last line without a newline; the main checkout stayed clean; and the linked checkout reported its own tracked and untracked changes. Main-checkout `git clean -fdx` retained both the linked Worktree's uncommitted file and the local exclude file unchanged.

Disposable tests with Git 2.55.0 on Linux confirmed:

- Git accepts this nested Worktree layout. Main-checkout status stays clean; the linked checkout independently reports tracked modifications and untracked files.
- The container ignore rule does not apply within the linked checkout. Ripgrep searches from inside that checkout find its files, while searches from the main checkout skip the contained Worktrees.
- Main-checkout `git clean -fdx` retains a valid nested Worktree but removes the container `.gitignore`, exposing the Worktree directory as untracked afterward.
- Main-checkout `git clean -ffdx` removes the container and nested Worktrees, including uncommitted files, leaving stale Git registrations. Recreation can restore committed contents, not deleted uncommitted work.

Git documents that `.gitignore` ancestry ends at the working-tree root and provides `$GIT_COMMON_DIR/info/exclude` for repository-local ignore rules. The accepted local-exclude rule survives ordinary working-directory cleanup. See [Git ignore rules](https://git-scm.com/docs/gitignore) and [Git clean](https://git-scm.com/docs/git-clean).

Ignore rules do not constrain tools that perform their own unrestricted recursive filesystem traversal. This does not change Git's checkout boundaries or Suru's rule to pass the actual Execution Directory to each Provider.
