# Keep a Workspace Registry fed by Sessions, discovery and clones

ADR 0027 refused a persisted repository registry so that repository identity stayed derived from real Session or preparation state; the Description and Icon rows then quietly became one, since a Workspace the user had described outlived its Sessions. We decided to make that registry explicit: the Server keeps a Workspace Registry, a row for every Workspace it has known — because a Session began there, because it was found directly within a Workspace Root, because a Sidekick cloned it there, or because someone described it — and lists Workspaces from the Registry alone, Sessions belonging to entries rather than being merged in beside them. Workspace Roots are a Server Setting holding any number of directories; discovery takes only the Repository roots directly within each, and a Sidekick's `clone_workspace` may place a clone directly within one and nowhere else. This supersedes the last paragraph of ADR 0027.

## Considered Options

- **Keep listing from two sources, Sessions and described rows, and hold discovered Workspaces only in memory.** Rejected: discovery would have to run before anything could be listed after a restart, a Root on an unmounted drive would make its Workspaces vanish rather than show as unavailable, and listing would keep the merge that is already the awkward part of the code.
- **Record the Repository URL in each entry so a Sidekick can be told an address is already cloned under another name.** Rejected for now: the URL is no part of a Workspace's identity, Git holds it anyway, and a later need can ask Git for it.
- **Offer cloning to every Agent.** Rejected: it writes to the user's disk outside the Agent's own Execution Directory, which no ordinary Tool does; every Agent is offered `list_workspaces` only, the first widening ADR 0042 foresaw.
- **Clone asynchronously and report back as a Sidekick Report.** Rejected: every Provider already allows a Broker call fifteen minutes, and a clone in flight would need a state of its own wherever Workspaces are listed.

## Consequences

- An entry stays while its directory can be read or a Session belongs to it. A scan that can read the directory's parent and finds the directory gone drops the entry with its Icon and Description; a parent that cannot be read keeps the entry, listed as unavailable. Removing a Root drops the entries discovered under it that no Session belongs to, so each entry records how it came to be known.
- The Reclaim sweep may now rediscover a Repository with no Sessions after a restart, through its entry; the limit ADR 0027 accepted no longer holds.
- Discovery runs whenever Workspaces are listed, reusing a scan only seconds old, as well as at start, on a change to the Roots, and after a clone. It derives no Icon or Description; a first Session does, as today, and a clone does at once.
- A partial clone that fails or times out is removed, so a Root never holds a half-made Workspace; an existing directory at the destination is refused rather than numbered, naming the Workspace there if one is.
