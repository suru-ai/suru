# Reach Remotes through a Relay that carries bytes and holds no trust

Extends ADR-0017, whose Pairings need the redeeming machine to reach the Serving machine's listener directly — workable where one person controls both machines and the network between them, unreliable wherever a firewall stands in the way. A Relay is a server a user or a company runs themselves, which every Server reaches by a connection it opens outward, so the one path to the Relay is all a network has to allow. The Relay joins two such connections and passes bytes between them and does nothing else: the mutual-key TLS of ADR-0017 runs end to end through it, each side still verifying the other's pinned key, so the Relay can neither read nor alter what the two Servers say and cannot stand in for either of them. A Pairing gives a Peer everything short of administration, which includes Agents running commands on the Serving machine; a Relay able to speak as a Server would, once compromised, be a way into every machine that connects through it, where one that only carries bytes can refuse or delay them and learn which machines reach each other, when, and how much.

A Relay is one more way of reaching a Serving Server and not a new relationship. An Invite may offer a Relay beside or in place of direct addresses, and Pairing, Remote, Peer, Origin, and Outlook mean what they did; reaching a Remote directly stays as ADR-0017 left it.

## Considered Options

- **A Relay that terminates TLS and proxies the HTTP/SSE API itself.** Rejected as above. It would let the Relay authorize each request, keep an account of what was done, and serve Clients of its own, and every one of those is the Relay holding what a Peer holds.
- **Direct connections found by NAT traversal, with a Relay only as the fallback.** Rejected for now: the point is a single path a company can allow, and traversal leans on the traffic such networks refuse. It can come later atop a Relay without changing what the Relay is trusted with.

## Consequences

- Serving through a Relay means a Serving Server keeps a connection to the Relay open so that it can be reached, where direct Serving opens a listener and waits.
- The Relay never parses what Servers say to each other, so it does not move with the Pairing protocol version, and a company's Relay need not be upgraded in step with Suru.
- Nothing that needs to read what Servers say can be offered from a Relay: it can record which machines connected, never what was done over the connection.
- ADR-0017's enrollment survives a Relay in the path only because the Pairing transport is TLS 1.3 alone. The enrollment token travels in the redeeming Server's certificate, which TLS 1.3 encrypts and sends only once the Serving Server's pinned key is verified; under TLS 1.2 that certificate would cross the Relay in the clear, and a Relay could spend the token for a key of its own.
