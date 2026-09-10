# Nerd Font icons

Implemented from the confirmed design.

## Agreed behavior

- Add a Client Setting named **Show icons** in **Appearance**, disabled by default and pinned as `appearance.showIcons` in Config Documents.
- Help text: “Show Nerd Font icons. Requires a Nerd Font in your terminal.”
- The Setting governs Nerd Font icons throughout Suru, including future additions.
- Initially decorate the location elements beneath the Landing composer and at the top-left of the open Session header.
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
