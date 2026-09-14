# Suru

Suru is an agent harness heavily drawing inspiration from tools like OpenCode and T3 Code.

The goal is to provide a high-performance, configurable, and provider-agnostic agentic coding experience,
initially running as a TUI, but potentially eventually with native GUIs across the various platforms.

## Managed Worktree Reclaim

Suru automatically **Reclaims** unused **Managed Worktrees** that it created under a Repository's
`.suru-worktrees` container. A Managed Worktree is **Reclaimable** in any of these cases:

- No Session or unfinished preparation references it. Suru checks this when it first observes the Worktree and
  immediately after deleting the last Session that referenced it.
- Every Session that references it has had no activity for the configured number of days. The clock starts at the
  latest activity across those Sessions; settlement does not determine that clock, and a Reclaim does not change a
  Session's settlement.
- A failed Worktree preparation has remained unfinished for the configured number of days.

A Reclaim never removes the main checkout, a linked Worktree created outside Suru's managed container, or a
Worktree used by a Working Session or Subagent. It also refuses tracked or staged changes, untracked files,
initialized submodules, and Git locks that Suru did not place. Ignored files may be removed because ordinary,
non-forced `git worktree remove` permits that. Suru does not substitute recursive directory removal when Git
refuses the operation.

The branch is retained unless Git can prove its current tip is fully merged into the current tip of the local source
branch Suru recorded when it created the Managed Worktree. If that source branch no longer exists, an existing local
remote-tracking ref may prove the merge; Suru does not fetch one. A Managed Worktree created before Suru recorded its
base and source branch keeps its branch. Reclaim leaves each affected Session's history and settlement unchanged,
marks its checkout Unavailable with the reason, and recovers the Worktree at its original path on the Session's next
Prompt.

To keep a Managed Worktree indefinitely, lock it with Git:

```sh
git worktree lock --reason "Keep for later" /path/to/worktree
```

Run `git worktree unlock /path/to/worktree` when it may become Reclaimable again.

The Server Setting `worktree.autoReclaim` appears in the **Source Control** tab between **Providers** and
**Experimental**. Its default is `14` days and its minimum is `1`. Set it to `"off"` to disable every Reclaim rule,
including failed preparations and the immediate check after deleting a last Session. The new value applies on the
next Reclaim pass without restarting the Server. The Server runs a pass shortly after startup and about hourly while
it remains running, even when no Client is attached.

The settings panel can pin the Setting, or it can be written as a nested value in `suru.jsonc` (with `suru.json`
accepted when the JSONC file is absent) under Suru's config root. For example:

```jsonc
{
  "worktree": {
    "autoReclaim": 30
  }
}
```

Use `"autoReclaim": "off"` to disable it. Invalid values are ignored and produce the same Config Document diagnostic
as other Settings.

Successful Reclaims are recorded only in the Server log, with the qualifying rule and actual branch outcome; Suru
does not show a notice or dialog. A failed Reclaim is logged and retried on a later pass. For a failed preparation,
Suru also removes its stored intent and ownership ref, applies the same branch rule, and logs the first line of any
withheld Prompt before discarding it.

The running Server remembers Repositories it has discovered and can retry a failed Reclaim on a later pass. Suru
does not persist a Repository registry, so after a restart it cannot rediscover a Repository with no Sessions and no
failed-preparation intent. Deleting a Repository's last Session triggers the immediate check while that Repository is
still known; if that attempt fails and the Server restarts before retrying, the Repository will not be found again on
its own.

## Persistent-server smoke check

1. Build Suru with `cargo build`.
2. Run `target/debug/suru` in two terminals.
3. Confirm both TUIs show the same server ID and PID.
4. Press `Ctrl+C` with an empty composer in both TUIs, wait a few seconds, then run `target/debug/suru` again.
5. Confirm the relaunched TUI shows the same server ID and PID, proving the shared server survived without connected clients.

The detached debug server intentionally remains running after this check. Its output is written to the debug channel's `server.log` under Suru's per-user state directory.
