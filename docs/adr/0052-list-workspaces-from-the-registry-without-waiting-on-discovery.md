# List Workspaces from the Registry without waiting on discovery

Every Workspace listing used to wait up to a second for the Workspace Roots to be discovered again, and up to another for each Registry entry's directory to be read, so the Workspace Picker took a second or two to fill on a slow machine, where each directory discovery reads costs a Git process or more. We decided a listing says what the Workspace Registry knows now and waits on nothing: discovery and the directory readings run after it, an entry not yet read is listed as available, and one whose reading goes unanswered too long is marked unavailable once it has. Whatever they change in the listing is pushed beside the Session catalog on the stream every Outlook's Client already follows, whose snapshot carries the listing when it connects, so a Client holds each followed Server's listing before the picker is first opened and the picker opens on it at once. This amends ADR 0051, whose Consequences had listing wait on a discovery reused only when seconds old; discovery still runs on every listing past that reuse window, but behind the answer.

## Considered Options

- **Shorten the waits.** Rejected: still not instant, and on the slow machines this is for, more entries would be marked unavailable only because their reading lost the race.
- **Cache the listing on the Client alone.** Rejected as the whole answer: every open would still show the Server's slow reply replacing the cached rows, and the first open after launch would wait as before. The Client keeps its copy too, so a Remote's picker is instant over a slow transport.
- **Push changes on the Server's own event stream.** Rejected: that stream is administration, open to the Client's own Server only, so a Remote's picker would never hear of a change.
- **Watch the Roots for changes instead of discovering on every listing.** Rejected for now: a new dependency with uneven behaviour on Windows network drives, while discovery behind the answer already costs the user nothing to wait for.

## Consequences

- Availability can lag: a Workspace whose directory has just gone is offered until the reading catches up, and choosing it is refused when its Server resolves it, as any refusal is.
- A Repository cloned by hand into a Root appears a moment after the picker opens rather than in its first rows, and the same holds for Peers and the Broker's `list_workspaces`, which share the one listing.
