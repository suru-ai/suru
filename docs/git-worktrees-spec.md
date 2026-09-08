## Problem Statement

Suru currently treats each directory as a separate Workspace and uses that same path to decide both where Sessions are listed and where their Agents execute. A user working in several linked Git Worktrees therefore sees related Sessions as unrelated Workspaces. Launching from a repository subdirectory fragments that grouping further. The Sidebar's reserved checkout line provides no branch or Worktree information.

Users need to find all work for one local Repository together, understand where each Session is operating, and start isolated work without manually naming branches or arranging checkout directories. Existing Sessions must remain in their actual working copies when regrouped. Missing Worktrees need useful recovery, and removing a Worktree must be independent of deleting conversation history.

This expands #169 from branch awareness into a complete Git Worktree workflow. Git is the only source control implementation in scope, but the design must permit future integrations such as Mercurial and Jujutsu.

## Solution

Group Sessions by their shared local Repository while preserving a separate, exact Execution Directory for each Session. The main checkout, linked Worktrees, and their subdirectories belong to one Workspace. Separate clones and nested Repositories remain distinct, even when they use the same remote address.

The Landing offers the current execution location, another existing Worktree, or a new Worktree. Choosing a new Worktree is all the user needs to do: on first Prompt submission, Suru captures the source checkout's current commit, generates a descriptive unique branch with a suru prefix, creates a managed checkout on the repository's disk, initializes submodules, and prepares the Agent. Worktrees are shared places that may contain several Sessions, not Session-owned disposable resources.

Sidebar rows show the current branch or detached commit together with a main/linked Worktree indicator. Shared Worktrees share one live checkout reading. Missing Worktrees are shown explicitly and recreated at their original location when prompted, where recovery is possible. Explicit removal preserves the branch and Session histories, blocks while the acting Server has associated Working Sessions, and offers warnings and a force option when needed.

## User Stories

1. As a user, I want Sessions from the main checkout and its linked Worktrees grouped into one Workspace, so that I can find all work on one Repository together.
2. As a user launching Suru in a repository subdirectory, I want that Session grouped with its Repository, so that directory depth does not fragment my Session lists.
3. As a user with existing Sessions, I want regrouping to preserve their exact Execution Directories, so that continuing a conversation never silently operates in another checkout.
4. As a user with separate clones of the same remote, I want them to remain separate Workspaces, so that distinct local copies are not confused.
5. As a user working with nested Repositories or submodules, I want the nearest Repository to determine membership, so that an independent nested checkout is not absorbed into its containing Repository.
6. As a user working outside source control, I want directory-based Workspaces to keep working, so that Git is not required for ordinary Sessions.
7. As a user with externally created Worktrees, I want Suru to discover and offer them, so that I can use my existing Git workflow.
8. As a user with detached Worktrees, I want them listed independently of branches, so that useful checkouts are not omitted.
9. As a user with a repository that has no commits, I want ordinary Sessions to work and unavailable creation actions to explain why, so that I can begin work without inventing a source commit.
10. As a user with a bare Repository, I want its linked Worktrees grouped under the bare root, so that this layout receives the same navigation support.
11. As a user selecting a bare root, I want Suru to require a working-copy choice before execution, so that the Agent does not run in repository metadata.
12. As a user whose main checkout cannot yet be located, I want related Worktrees to remain grouped and the unknown location made clear, so that incomplete discovery does not split my Workspace.
13. As a user, I want discovering the main checkout later to update the Workspace label without changing its identity, so that Sessions do not move between duplicate entries.
14. As a user switching Workspaces, I want my Client to restore its last Execution Directory for that Workspace on that Server, so that I can return to where I was working.
15. As a user selecting a Workspace for the first time, I want its main Worktree selected when available, so that there is a predictable starting location.
16. As a user launching Suru from a specific directory, I want that directory preserved until I choose otherwise, so that launch context is respected.
17. As a user choosing another existing Worktree, I want the new Session to begin at that checkout's root, so that changing checkouts has a clear execution location.
18. As a user who needs a checkout subdirectory, I want to select it explicitly, so that grouping and execution location remain independent.
19. As a user whose remembered directory has disappeared, I want it to remain visibly selected until recovered or changed, so that Suru does not silently choose another location.
20. As a user, I want multiple Sessions to be able to share a Worktree, so that an existing working copy can support related conversations.
21. As a user starting isolated work, I want a single new-Worktree choice on the Landing, so that I do not have to fill in branch, revision, or destination fields.
22. As a user changing my mind on the Landing, I want the new-Worktree choice to create nothing until submission, so that abandoned drafts do not create checkouts.
23. As a user submitting a new-Worktree Prompt, I want its starting point to be the source checkout's current commit at submission, so that Suru does not use a stale selection or fetched remote revision.
24. As a user, I want generated branches to have a recognizable suru prefix, a short description, and a unique suffix, so that I can identify them in Git without naming them manually.
25. As a user, I want branch descriptions derived locally from the first Prompt, so that creation needs no additional AI request or later rename.
26. As a user creating a Worktree from a subdirectory, I want the new Session to begin at the new checkout root, so that it follows the same rule as selecting another Worktree.
27. As a user, I want managed Worktrees stored on the repository's disk, so that an unrelated Suru data-directory location does not determine where my checkouts live.
28. As a user creating from a linked Worktree, I want the destination anchored to the main Repository root, so that managed checkouts do not become nested inside arbitrary linked checkouts.
29. As a user, I want Suru's generated checkout container ignored locally without changing tracked files, so that using Suru does not create unrelated repository changes.
30. As a user with existing local ignore rules, I want those rules preserved, so that Suru does not disrupt my other tooling.
31. As a user of multiple Channels, I want their generated destinations kept separate, so that development and release runs do not allocate the same managed checkout.
32. As a user of submodules, I want them initialized recursively before Agent startup, so that committed source and Skills inside them are available.
33. As a user encountering creation or submodule failure, I want a useful error, a preserved draft, and a reusable checkout when one was already created, so that retrying does not lose work or create duplicates.
34. As a user editing a draft after preparation, I want Suru to reuse the prepared Worktree, so that a changed Prompt identity does not allocate another checkout.
35. As a user selecting explicit Skills before a Worktree exists, I want Suru to refresh the destination Skill Catalog and require reselection before delivery, so that a Skill is not silently retargeted into a different context.
36. As a user sending a Prompt without explicit Skills, I want successful preparation to proceed directly to the Agent, so that ordinary new-Worktree creation remains automatic.
37. As a user, I want a Session's Execution Directory fixed from its first Turn onward, so that later activity and Provider Resume State remain associated with the same location.
38. As a user wanting to work in another checkout, I want to start another Session there, so that existing conversation assumptions are not silently moved.
39. As a user reading the Sidebar, I want each active Session row to show the current checkout branch or detached commit and a main/linked indicator, so that I know where its Agent works.
40. As a user switching branches outside Suru, I want every Session sharing that Worktree to reflect the change automatically, so that the Sidebar describes current checkout state rather than an old association.
41. As a user with a missing or unreadable Worktree, I want that state marked explicitly, so that an unavailable checkout is not confused with a directory outside source control.
42. As a user prompting a Session whose Worktree disappeared, I want Suru to recreate it at its original path, so that I can resume without manually rebuilding the checkout.
43. As a user recovering a branch Worktree, I want its retained branch's current tip used, so that commits made since its creation are preserved.
44. As a user recovering a detached Worktree, I want its last-known commit restored, so that recovery does not invent a branch or revision.
45. As a user encountering an occupied branch, deleted branch, conflicting destination, or inaccessible repository data, I want recovery to stop before Agent startup with a clear explanation, so that work does not begin in an incorrect location.
46. As a user of an externally created Worktree, I want the same known-path recovery behavior, so that recovery does not depend on Suru having created the checkout.
47. As a user settling or deleting a Session, I want its Worktree left alone, so that conversation lifecycle does not unexpectedly remove files.
48. As a user removing an idle Worktree, I want to keep its branch and Session histories, so that cleanup does not require deleting useful conversations or committed work.
49. As a user removing a shared Worktree, I want the acting Server to show its affected Session count and block while any associated Session or surviving Subagent is Working, so that removal does not interrupt work it knows about.
50. As a user removing a Worktree, I want dirty, untracked, ignored, locked, and submodule-related conditions explained and a force option where supported, so that I can make the removal decision explicitly.
51. As a user later prompting a Session after explicit Worktree removal, I want ordinary recovery rules to apply, so that removing files does not make retained history permanently unusable.
52. As a Remote user, I want repository discovery and filesystem actions performed by the owning Server, so that local paths and tools are never applied to the wrong machine.
53. As a user with matching directory spellings on different Servers, I want those Workspaces kept distinct, so that remote and local work are not conflated.
54. As a user restarting Suru, I want Repository associations, execution locations, recovery state, and resumable conversations retained, so that the workflow survives process replacement.
55. As a user with substantial history, I want grouping and live checkout labels without loading every Transcript, so that Session discovery remains responsive.
56. As a user on Windows, macOS, or Linux, I want the same behavior with native paths and short, bounded waits, so that Worktree support is reliable on every supported platform.
57. As a future source control integration author, I want capability-based interfaces independent of the Agent Provider interface, so that another system is not required to imitate every Git concept.

## Implementation Decisions

1. **Separate grouping from execution.** Evolve the Workspace, Session metadata, durable storage, listing, and path-resolution contracts to carry Repository-based grouping separately from the exact Execution Directory. Preserve old Session execution paths, Provider Resume State, and histories when regrouping. Keep a missing legacy path unresolved when no evidence of its Repository membership survives; never guess membership from names or remote URLs.

2. **Use Repository identity rather than a presentation path.** For Git, discover the nearest repository and canonicalize its common metadata directory to recognize related Worktrees. Separate clones, nested Repositories, and distinct Origins remain separate. A main-root label, a bare-root label, or a metadata-location label with “main checkout unknown” is presentation, not identity. Discovering the main root later must not create another Workspace.

3. **Keep Git behind a source control boundary.** Add typed Repository discovery, checkout observation, and supported working-copy operations behind interfaces separate from Agent Providers. Capabilities determine available actions. Distinguish unsupported operations, unavailable repositories, and directories outside source control. Git is the first implementation; avoid making every future integration require branches or Git Worktrees. A loader and public plugin API are deferred.

4. **Keep authority on the owning Server.** Clients consume typed results and invoke semantic actions. Discovery, observation, preparation, recovery, removal, and path interpretation execute on the Server that owns the Workspace. Remote operations use the established routing and Origin model. Codex, Copilot, and Claude receive the same resolved execution-location contract through their existing Provider interfaces.

5. **Represent Worktrees independently of branches.** Discover main and linked Worktrees, including external, detached, unborn, and bare-backed checkouts. Store enough association data to distinguish checkout roots from Session subdirectories. A Worktree may be shared by several Sessions and is never owned by Session lifecycle.

6. **Preserve directory interpretation.** Canonicalize readable paths on their owning Server. Retain existing tolerant startup behavior for paths that cannot be canonicalized and explicit path-entry rejection for missing or non-directory inputs. Presentation continues to abbreviate the owning Server user's known home directory and preserve native path separators.

7. **Update navigation as one model.** Workspace selectors group by Repository; the Workspace Picker retains its current-first, then recent-work ordering and known-Workspace scope. Selecting a Workspace restores the Client's last Execution Directory for that Server and Workspace, defaults to the main Worktree where appropriate, and opens the Landing without moving the open Session or changing Sidebar scope. The Sidebar's own path entry continues to switch and narrow together, resolving relative paths from the execution context. Bare roots select only a Workspace and require a working-copy choice.

8. **Keep Worktree choice on the Landing.** Offer the current location, existing Worktrees, and a new-Worktree intent. Another existing Worktree or a newly created one starts at its root; subdirectory selection stays explicit. Merely choosing new-Worktree mode performs no filesystem mutation. A Session's Execution Directory becomes fixed from its first Turn onward; starting elsewhere creates another Session.

9. **Create from the local source commit on first submission.** Capture the source checkout's current commit once for a preparation, before creating its new branch and Worktree. No base-revision, branch-name, or destination form is offered. Do not fetch a remote base, copy uncommitted source changes, or copy ignored/local configuration files. Retrying the same preparation retains its captured commit even if the source checkout moves.

10. **Generate branch names locally.** Use the suru prefix followed by a slash, a short description derived from the first Prompt, and a unique suffix separated by a hyphen. Produce valid Git refs and portable directory names, including for empty-after-normalization, non-ASCII, long, and collision-prone input. No additional AI request or subsequent automatic rename is part of naming. Never reset an existing branch to resolve a name collision.

11. **Use repository-local managed storage.** Create a hidden directory named .suru-worktrees at the resolved main root, then a Channel directory and a generated Worktree-name directory beneath it. Creation initiated in a linked checkout still uses the main root. For a bare Repository, place that same container under the bare root. Keep storage on the repository's disk and do not silently fall back to Suru's general data directory. If a non-bare main root cannot be found, disable managed creation until it is located while retaining existing Session and Worktree selection support.

12. **Ignore the managed container locally.** Add a root-anchored directory rule for .suru-worktrees to Git's repository-local exclude file in the common metadata directory, preserving all existing entries and correctly handling an existing last line without a newline. Do not change tracked ignore files or depend on a container-local ignore file that ordinary cleanup can delete. Channel directories separate generated destinations; they do not isolate the files of an existing Worktree selected across Channels.

13. **Make preparation resumable and distinct from Prompt identity.** Retain a stable preparation identity and recoverable progress across retries, edited drafts, Skill reselection, interrupted requests, and a restart after filesystem creation. A changed Prompt ID must not allocate another checkout. Validate any previously created destination before reuse; do not overwrite unrelated contents. Already-created Worktrees remain available for reuse or explicit removal after later failure.

14. **Prepare before Agent startup.** Initialize submodules recursively during both creation and recreation, and require success before Provider startup. Failure retains the Worktree and offers retry. Before Prompt admission, preparation failure preserves the draft. After admission, existing failed-Turn semantics apply; a later Provider failure does not undo the Session or Worktree.

15. **Resolve explicit Skills in the real destination.** Existing Skill identities include Execution Directory for all three Agent Providers, and discovery requires that directory to exist. After creation, refresh the destination Skill Catalog and require reselection of every explicitly bound Skill before delivery. Preserve the draft and reuse the prepared Worktree. Do not silently retarget bindings by name. Without explicit Skills, successful preparation proceeds directly to ordinary admission and Agent startup. Retain existing revalidation before native delivery.

16. **Observe live Checkout State.** The active Sidebar row's reserved third line shows the current branch, or a short commit identifier when detached, with a main/linked indicator. Sessions sharing a Worktree share the same current reading. External branch changes appear automatically within roughly two seconds while relevant state is visible. Share observation work instead of launching one Git observation per Session; use the existing client update flow to refresh presentation. This introduces no new Setting initially.

17. **Distinguish unavailable from absent source control.** Known checkout state that cannot be read is explicitly unavailable. A directory outside supported source control leaves the checkout line blank. Ordinary Sessions remain usable wherever execution is possible if Git is unavailable or a repository is unborn; creation reports why a usable source commit/checkout is missing. Keep a missing remembered Execution Directory visibly selected until recovered or explicitly changed rather than substituting another path.

18. **Persist recovery facts, not branch history.** Remember the latest known branch and commit per Worktree for recovery across restarts. This is not a historical branch association for each Session, and the Sidebar must not present stale recovery metadata as live checkout state. Repository associations and checkout summaries must remain accessible without hydrating Session histories.

19. **Recover missing linked Worktrees before execution.** Apply recovery to any known linked Worktree referenced by a prompted Session, including external ones and those explicitly removed through Suru. Restore its original path from its last-known branch at that branch's current tip. For a detached Worktree, use its last-known commit. Validate an existing destination rather than treating path existence alone as proof of identity. Preserve the Session's exact subdirectory and stop if that Execution Directory remains unavailable after checkout restoration.

20. **Fail recovery without redirecting execution.** Deleted or occupied recovery branches, unavailable commits, inaccessible repository metadata, locked registrations, and conflicting destinations produce clear errors before Agent startup. Do not guess another branch, reset a branch, override a recovery lock, or run in the main checkout instead. Handle only the required stale registration rather than making recovery a reason to prune unrelated Worktrees. Recreating committed contents cannot restore deleted uncommitted or ignored files.

21. **Keep removal explicit and independent.** Settling or deleting Sessions never removes Worktrees. The acting Server counts its associated Sessions and blocks removal while any is Working, including surviving Subagents. This authority does not claim knowledge of another Channel's Session catalog or external processes. For idle references, confirm using the affected Session count, remove only the selected Worktree, retain its branch and Session histories, and mark the execution location unavailable. Later prompting invokes recovery.

22. **Offer informed force removal.** Explain Git refusals for tracked modifications, untracked contents, locks, and initialized submodules, and offer a distinct force action where Git supports it. Disclose ignored contents before confirmation because ordinary removal can delete them too. Force never overrides the acting Server's Working-Session guard. The main checkout cannot be removed through this operation. Recheck state at mutation time and never substitute recursive deletion of an unrelated directory.

23. **Coordinate shared mutations.** Serialize conflicting Suru operations on a Repository and coordinate prompt preparation/startup with removal so their checks cannot race into deleting an admitted execution location. Git state may still change externally; revalidate state and handle Git failures without misdirecting execution or duplicating resources. Keep recoverable preparation progress durable enough to reconcile a crash between filesystem mutation and metadata persistence.

24. **Respect current storage and extension architecture.** Evolve protocol and durable metadata where needed without backward-compatibility scaffolding for this early application, while preserving readable existing Session data and execution paths as explicitly required here. Maintain lazy Session-history hydration, Provider-opaque Resume State, and the distinction between Server facts and Client presentation state. Use semantic command IDs and the existing typed UI extension conventions rather than introducing a plugin loader or generic rendering escape hatch.

## Testing Decisions

1. **Primary boundary: the existing Client–Server contract.** Extend authenticated Session/Workspace requests and managed-client streaming integration tests using real temporary Git repositories, an isolated Server, and the existing controlled Provider runtime. Drive observable requests, responses, events, restart behavior, and filesystem outcomes. Do not introduce a separate test-only application boundary or assert private data structures, SQL layout, exact subprocess sequences, or cache implementation.

2. **Use the highest boundary that proves the behavior.** A good test demonstrates a user's invariant: related Sessions list together without changing Provider execution paths; one submission creates one checkout; invalid recovery never starts a Provider; removal retains history and branch. Exercise the full Server path for discovery, preparation, recovery, removal, and persistence. Use actual Git for its filesystem semantics rather than mocked command output as the primary evidence.

3. **Reuse existing integration prior art.** Existing tests already cover idempotent Session creation using Prompt identities, asynchronous Provider startup failures, canonical Workspace filtering, persisted metadata and Resume State, first-access history hydration, Skill validation and revalidation, controlled Working/Subagent state, and Remote path resolution and streaming. Extend these patterns to the new grouping and preparation contracts rather than recreating their harnesses.

4. **Keep UI verification focused.** Extend the existing Application event/rendering tests for Workspace Picker behavior, Sidebar labels/scopes, Remote path labels, composer draft restoration and stale Skills, Landing Worktree choice, unavailable states, and removal confirmation/force interactions. Assert semantic actions and meaningful visible output. Do not duplicate the entire Git lifecycle through pixel snapshots or every possible UI event sequence.

5. **Verify all Agent Providers at their existing native boundary.** Reuse Codex, Copilot, and Claude scripted-binary integration patterns to prove startup/resume receives the exact prepared Execution Directory and no native execution begins before checkout and explicit Skills are ready. Keep most lifecycle scenarios Provider-neutral in the controlled-runtime suite instead of multiplying them across all three adapters. Do not require live credentials or model calls.

6. **Cover identity and discovery with real layouts.** Include main and linked checkouts, subdirectories, external Worktrees, detached and unborn states, bare repositories, separately located metadata, same-remote independent clones, nested repositories/submodules, symlinked spellings where supported, and unknown-main discovery followed by a known-main label. Assert Repository grouping and independent Execution Directories through Client-facing listings and requests.

7. **Cover existing data and restart behavior.** Restore previously stored path-only Sessions, preserve execution paths and opaque Resume State, and verify grouping where membership can be discovered. Preserve unresolved missing legacy paths when it cannot. Assert listings and checkout updates do not hydrate unopened Transcripts. Persist latest-known recovery state and exercise a restart after Worktree creation but before preparation completion.

8. **Cover navigation and no-execution locations.** Verify per-Client/Server/Workspace remembered locations, preserved launch subdirectories, root selection when changing Worktrees, bare-root selection requiring a working copy, and missing remembered directories that remain explicit. Same directory spellings on separate Origins must not merge or cause local filesystem operations on a Remote's behalf.

9. **Cover automatic creation and naming.** Toggling the Landing option must not mutate Git. First submission must use the current local commit and a unique valid generated branch, with no remote-base selection or copying of uncommitted source files. Test source-commit changes before submission and after preparation starts, invalid/long/non-ASCII description input, collisions, and creation from a linked checkout anchoring at the main root.

10. **Cover exact storage and ignore outcomes.** Verify the repository-local, Channel-separated layout, including bare-root placement and refusal to create when the required non-bare main root is unknown. Preserve existing local exclude entries, including missing final newlines. Assert the main checkout remains clean, the linked Worktree sees its own tracked/untracked changes, and ordinary force-cleaning from the main checkout preserves the nested Worktree and local exclude rule. Use disposable repositories for all destructive fixtures.

11. **Cover interrupted preparation and corrected drafts.** Inject failures after checkout creation and during submodule initialization, destination Skill discovery, and metadata persistence. Retry unchanged and edited Prompts and reselected Skills; assert reuse of the same prepared Worktree and preservation of the draft before admission. Verify that successful admission followed by Provider failure follows existing failed-Turn behavior rather than deleting resources.

12. **Cover destination Skills and submodules.** Use local, temporary submodule repositories, including nested submodules, to avoid external network dependencies. Assert initialization succeeds before Provider startup, failure is retryable, source-checkout Skill bindings cannot pass as destination bindings, and selecting destination Skills does not allocate another Worktree. A Prompt without explicit Skills must proceed without a Skill-reselection step.

13. **Cover live observation through events.** Switch branches and detach externally, remove or make a checkout unavailable, and verify the relevant Client-facing reading changes within an injected interval. Multiple Sessions sharing a Worktree must agree. Check that absent source control stays distinct from unavailable known state and that observation does not require loading histories.

14. **Cover recovery and blocking failures.** Remove managed and external linked Worktrees, advance retained branches, restart the Server, and prompt retained Sessions. Assert recreation at the original path from the current branch tip or remembered detached commit. Deleted branches, branches occupied elsewhere, missing commits, locks, missing repository data, replacement directories, and missing Session subdirectories must produce errors before any Provider starts.

15. **Cover explicit removal and shared activity.** Demonstrate that Session settlement/deletion never removes files, Working associated Sessions and surviving Subagents block removal, and idle references retain histories and branches. Test tracked, untracked, ignored, locked, and initialized-submodule conditions, their visible warnings, the force action, and the main-checkout exclusion. Prompt retained Sessions afterward and verify recovery. Scope the count and guard to the acting Server as specified.

16. **Cover races at the Server boundary.** Exercise duplicate creation requests, changed draft identities, concurrent prompts recovering a shared missing Worktree, and removal racing with preparation or Provider startup. Assert one valid destination, no duplicate admitted work, no deletion of an admitted execution location, and no overwrite of unrelated filesystem state. Reuse existing controlled scheduling/failure mechanisms; add only the smallest boundary-level injection needed for deterministic crash windows.

17. **Make every fixture portable and fast.** Run on Windows, macOS, and Linux with temporary paths rooted according to each platform. Gate genuinely platform-specific symlink, permission, or process behavior. Inject millisecond-scale observation, timeout, and retry intervals rather than waiting production delays. Run implementation tests with cargo nextest run and the repository's applicable checks.

18. **Treat exploration as evidence, not completed implementation tests.** Disposable Linux checks with Git 2.55.0 verified nested main/bare Worktree layouts, ignore boundaries, local-exclude preservation, removal conditions, and cleanup behavior. These findings guide the fixtures; they do not establish Windows/macOS correctness or mean this feature is already implemented.

## Out of Scope

- Mercurial, Jujutsu, and other source control implementations; their future compatibility is an interface constraint only.
- A plugin loader, public plugin API, or new generic UI rendering system.
- A general Git client for commits, staging, merge/rebase workflows, remote management, or arbitrary branch switching inside an existing checkout.
- User forms or Settings for generated branch names, base revisions, or managed destinations.
- Fetching a remote base, AI-generated branch names, automatic branch renaming, or historical per-Session branch tracking.
- Moving a Session's Execution Directory after its first Turn begins.
- Automatic Worktree deletion when a Session settles or is deleted, automatic branch deletion, or forced removal of the main checkout.
- Dependency installation, project setup commands, and copying uncommitted or ignored/local files into a newly created checkout. Recursive Git submodule initialization is included.
- Recovering deleted uncommitted files, inventing a replacement for a deleted recovery branch, or rebuilding lost repository metadata.
- Treating channel-specific managed directories as filesystem isolation for shared existing Worktrees, or discovering another Channel's Sessions and arbitrary external processes for the removal guard.
- New user-facing Settings for the initial observation and creation behavior.

## Further Notes

This is the agreed expansion of #169 following the design interview. ADR-0023 records the separation of Repository-based Workspace grouping from exact Session execution and refines ADR-0014's Skill Catalog scope. Preserve the established Server/Origin routing, durable Session storage, and lazy-history decisions.

T3 was used as a reference, not a specification. Its project/execution-path split and original-path recovery from a retained branch's current tip are useful precedents. Suru deliberately differs by grouping one local Repository rather than remote-URL identity, taking the source checkout's current commit, supporting detached recovery, validating destinations, blocking execution on failed preparation/recovery, and avoiding AI naming or asynchronous setup commands. T3's partial-creation artifacts and branch-rename metadata races informed the retry requirements.

The implementation can be split into five dependent slices:

1. Repository and execution identity: typed source control boundary, Git discovery, persistence, and existing-Session regrouping.
2. Existing Worktree navigation and live Checkout State, depending on slice 1.
3. Managed first-Prompt preparation, storage, generated naming, submodules, Skills, and all three Agent Providers, depending on slices 1–2.
4. Missing Worktree recovery and pre-start failure handling, depending on slices 1 and 3.
5. Explicit Worktree removal, force handling, retained references, and later recovery, depending on slices 2–4.

Git's ordinary removal can delete ignored files. Nested Worktrees survive ordinary force-cleaning in the validated layout, but double-force cleanup can delete them and their uncommitted contents; the local exclude rule is not a backup or deletion barrier. Surface the specified warnings and recovery limitations without promising recovery of uncommitted work.
