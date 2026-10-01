# Reach Remotes from a Sidekick's own Server, and never forward the Broker

A Sidekick works on a Remote's Sessions and Workspaces, but its Agent only ever speaks to its own machine's Broker: the Tools themselves understand Origin, and the local Server carries each act to the Remote over the Pairing exactly as it carries a Client's. The Broker stays loopback-only, as ADR 0034 left it, because its tokens name one machine's Provider Sessions and mean nothing on another.

So a Sidekick can do on a Remote precisely what a paired Peer's user already can and no more. The Serving side's existing refusal of administration to a Peer is what keeps a Remote's Settings out of reach, and Memories stay with the Server that holds them. What a Sidekick sends a Remote arrives attributed to a Sidekick on that Peer, and the Remote refuses such an act on its own Sidekick Workspace.

## Considered Options

- **Forward the Broker to Peers, so a Sidekick calls the Remote's own Tools.** Rejected: it would put an Agent on one machine in direct possession of another machine's Broker, including its Settings and Memory Tools, and would need a second notion of caller for a token that is not a local Session's.
- **Keep a Sidekick to its own Server.** Rejected: the user's work already ranges over Everywhere, and a Sidekick blind to Remotes could not answer for most of it.

## Consequences

- A Pairing now lets a Peer's Agent, not only its user, act on the Serving machine's Sessions. No gate was added; attribution in the Transcript is the visibility.
- Sidekick attribution on Prompts and Answers is part of what Servers say to each other, so it moves with the Pairing protocol version.
