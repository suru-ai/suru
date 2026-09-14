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
