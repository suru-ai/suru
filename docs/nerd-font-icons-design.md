# Nerd Font icons

Implemented from the confirmed design.

## Agreed behavior

- Add a Client Setting named **Show icons** in **Appearance**, disabled by default and pinned as `appearance.showIcons` in Config Documents.
- Help text: “Show Nerd Font icons. Requires a Nerd Font in your terminal.”
- The Setting governs Nerd Font icons throughout Suru, including future additions.
- Decorate the location elements beneath the Landing composer and at the top-left of the open Session header.
- Prefix Provider names beneath the Landing and Session composers and in the Providers settings tab: `nf-cod-openai` for Codex, `nf-cod-claude` for Claude, and `nf-cod-copilot` for Copilot.
- Preserve existing text except the `(worktree)` suffix described below. Place prefix icons to the left of their text, in the same color, separated by one space.
- Use `nf-cod-folder` for the workspace/path and `nf-md-monitor` for the Remote when present.
- Use `nf-cod-git_branch` before a named main-Worktree branch. For a named linked-Worktree branch, use `nf-cod-worktree` instead of the branch icon and omit the `(worktree)` suffix.
- Use `nf-cod-git_commit` for detached Checkout State, including detached linked Worktrees. Other checkout states have no icon.
- Prefix the pending “New Worktree on submit” hint with `nf-cod-worktree`, retaining its text.
- When icons are disabled, retain the existing text presentation, including `(worktree)`.
- Include icon widths in existing layout and clipping behavior, with no additional rule to drop icons when space is tight. Existing responsive rules still hide icons together with their associated labels.

## Domain alignment

Remote means a paired Serving Server, not a Git remote. The existing location labels retain their meaning: a Workspace groups work, while an Execution Directory identifies where a Session runs. The checkout label can describe a branch, a detached commit, or unavailable Checkout State.

The Landing currently displays detached and unavailable Checkout State; the Session header only displays named branches. The header presents the Workspace name while the Landing presents a path. Existing responsive header rules hide Workspace and branch together at narrow widths while retaining the Remote.

This feature uses existing domain terms and the existing Setting model. No new glossary term or architectural decision is needed so far.

## Validation

The confirmed test boundaries are the existing settings/config integration tests and rendered TUI tests. They cover the Appearance panel, Config Document persistence and settings delivery, and the Landing and Session header presentations. Run focused tests during implementation and the full suite at completion.

## Active Sidebar rows

- The first line presents the Remote name, when present, followed by ` · ` and the Workspace name. Show the Remote in every scope, including a single Workspace, and retain these names and their placement when icons are disabled.
- With **Show icons** enabled, prefix the Remote with the monitor icon and the Workspace with the folder icon, using the same spacing and colors as the Landing.
- Remove the active row's Remote tag from its Title line. Settled rows retain their existing presentation.
- The checkout line follows the Landing's icon rules: branch for a main Worktree, worktree for a linked branch, commit for detached state, and no icon for unavailable state. A worktree icon replaces `(worktree)`; with icons disabled, retain the suffix and its existing truncation behavior.
- Keep status and age in the right slot, truncating the location from the right as needed. Icon widths count toward the available space.
- Validate icons enabled and disabled, Remote placement across scopes, local rows, checkout states, and Title truncation with rendered Sidebar tests.

These additions reuse existing domain terms and need no new ADR.

## Icons

- An **Icon** is one Nerd Font glyph standing for a Session beside its Title, or for a Workspace wherever it is named — a typed property carried beside the text it stands beside, never written into it, so a listing search matches words rather than glyphs. Both kinds are chosen from the **Icon Catalog**, Suru's own fixed table of named glyphs, and remembered by Catalog name rather than codepoint. See the **Icon**, **Icon Catalog**, and **Icon Picker** glossary entries in `CONTEXT.md`, and ADR 0028, for the full account of why a name rather than a codepoint.
- An Icon is drawn wherever the Session or Workspace it stands for is named — a Sidebar row, a selector entry, a Workspace Picker row, the Landing, an open Session's header — and is governed by the single **Show icons** Setting this document's earlier sections describe: disabled, nothing here draws differently from before Icons existed, but derivation and storage continue underneath regardless. A Workspace's Icon, where it has one, replaces the plain folder glyph that otherwise stands for it; a Workspace with none is drawn beside that folder glyph exactly as if Icons carried no such property at all.
- **Derivation.** A Session's Icon is derived together with its Title, in the same Title Errand, the moment its first Prompt is admitted. A Workspace's Icon is derived by a second Errand on that same task, asked only where the Workspace still carries none, from the Workspace's presented name and — where its main root is known — the opening of its README; it is retried by the next Session created there until one lands. Both Errands answer through whichever Provider the `derivation.errand` Setting names, and both are skipped together where that Setting is off or where the Session itself selects no Provider. Either Icon may also fail to derive at all, in which case the Session or Workspace simply carries none until something fills it.
- **Choosing.** A user may choose either kind of Icon by hand from the Icon Picker — the centered, searchable grid described above, reached from a Session's Sidebar row or open header, a Workspace's Sidebar selector entry, or a Workspace Picker row, in each case through that row's own context menu's Choose icon item. The Icon Picker opens, and each entry point's item is offered, only while Show icons is on; with it off the picker's ways in are withheld and its command does nothing if invoked regardless. Choosing always replaces whatever Icon stood there — derived, chosen before, or absent — there is no way to clear one or to restore the folder glyph once an Icon has been chosen.
- **Fills absence.** Derivation and choice interact by one rule: a derived Icon only ever fills an absence, so a Session or Workspace that already carries an Icon — by an earlier derivation or a user's own choice — is left untouched by any later derivation. A user's choice, by contrast, always stands: it replaces whatever was there and is never itself overwritten by a derivation racing behind it. This is what lets a reader pick an Icon before an Errand has even answered without a slower derivation clobbering it moments later.
- Every client sees the same Icon at the same time: setting or deriving one publishes a catalog change (`SessionTitleChanged`'s `icon` field for a Session, `WorkspaceIconChanged` for a Workspace) that every connected client — the one that chose it and every other — applies the same way, whether or not the Session or Workspace in question is open there.

These additions reuse existing domain terms and glossary entries already covering Icons, the Icon Catalog, and the Icon Picker; no new ADR is needed.
