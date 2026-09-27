# Derive a brokered Subagent's posture through a fixed table between Providers' values

A brokered Subagent on a Provider other than its spawner's takes its Approval Posture from a fixed table: the spawner's native value maps to one of four levels, and the level maps to one canonical value of the child's Provider. The child's posture is re-derived whenever the spawner's changes, as native children's already is, and is never editable on the child; a brokered child on the same Provider inherits verbatim. ADR 0026 rejected a Suru-wide posture because mappings between Providers are lossy, and that stands for the user-facing Settings, which remain native; this table is the "convenience layered on top" that 0026 allowed, applied only where there is no native value to inherit. Where a cell is lossy the table errs toward asking, because an unwanted Intervention is recoverable and an unwanted permission is not.

| Level | Meaning |
|---|---|
| 1 | every edit or command that needs consent asks |
| 2 | routine work in the workspace runs; going beyond it asks |
| 3 | nothing asks; work outside the sandbox or allowlist is refused |
| 4 | nothing asks; everything is allowed |

| Provider | Native value | Level |
|---|---|---|
| Claude | default | 1 |
| Claude | acceptEdits, auto | 2 |
| Claude | dontAsk | 3 |
| Claude | bypassPermissions | 4 |
| Codex | untrusted with any sandbox; on-request with read-only | 1 |
| Codex | on-request with workspace-write or danger-full-access | 2 |
| Codex | never with read-only or workspace-write | 3 |
| Codex | never with danger-full-access | 4 |
| Copilot | ask | 1 |
| Copilot | allowAll | 4 |

| Level | Claude | Codex | Copilot |
|---|---|---|---|
| 1 | default | untrusted, workspace-write | ask |
| 2 | acceptEdits | on-request, workspace-write | ask |
| 3 | dontAsk | never, workspace-write | ask |
| 4 | bypassPermissions | never, danger-full-access | allowAll |

## Considered Options

- **The child follows its own Provider's Setting.** Rejected: a Codex child under a Claude parent the user set to bypassPermissions would fall back to the Codex default and start asking.
- **A posture root walk that skips other Providers.** Rejected for the same reason at one remove: a Codex child with no Codex ancestor still had nothing to inherit.

## Consequences

- Copilot has no unattended-contained value, so a Copilot child under a dontAsk or never/workspace-write parent raises Interventions rather than running free.
- Claude's auto sits at level 2 because its classifier still asks; Codex's read-only sandboxes collapse into levels 1 and 3 because neither other Provider can say read-only.
- The table is a constant, not a Setting; changing a cell is a code change recorded here.
