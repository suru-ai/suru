# Separate Workspace grouping from Session execution

Workspaces group Sessions by shared local Repository identity, including linked Worktrees and subdirectories, while each Session retains its exact Execution Directory. The main root presents the Workspace when known; discovering it later changes presentation without changing identity. This lets users find related Sessions together without redirecting Agent execution or Skill discovery into another working copy; existing Sessions keep their execution paths when regrouped. Separate clones and nested Repositories remain separate Workspaces, even when their remotes match, and directories outside source control remain directory-based Workspaces.

This refines ADR-0014's use of “Workspace” for Skill Catalog scope: that scope remains the Provider's actual Execution Directory, rather than the broader repository grouping.

A bare Repository groups its linked Worktrees under the bare root, which is never itself an Execution Directory. Workspace selection and execution-location selection are distinct, so such a Workspace requires choosing a working copy.
