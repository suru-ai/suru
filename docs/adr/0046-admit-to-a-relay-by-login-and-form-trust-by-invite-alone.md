# Admit to a Relay by login, and form trust by Invite alone

A Relay (ADR-0045) serves many users, so it requires a login and joins two Servers only where both are logged in as the same user: each user reaches their own machines through it and no one else's. Login decides who may use the Relay and nothing more. A Pairing is still formed only by redeeming an Invite, whether it travels through the Relay or directly, and no Server trusts another on the word of the Relay, or of an identity provider, that the two belong to one user. Login is by GitHub first, behind an interface other identity providers — Okta, Google — can later stand behind.

Were login to form trust, a stolen GitHub login, or a Relay that lied about which key is whose, would hold what a Peer holds on every machine of that user. The price is that a machine joining through a Relay is both logged in and paired by Invite; that was judged worth paying for a step taken once per machine.

## Considered Options

- **Machines logged in as one user find and trust each other on their own.** Rejected as above, though it asks the least of the user: it makes the Relay and the identity provider each able to add a machine to a user's Pairings, which is the trust ADR-0045 keeps from the Relay.

## Consequences

- The same-user rule is a second check beneath the Invite, never the source of trust. It also means two users cannot pair their machines through a Relay; pairing directly, which knows nothing of users, stays open to them.
- Losing a login — at the Relay or at the identity provider — takes a user's machines off the Relay and ends no Pairing: one that also has a direct address goes on working, and ending a Pairing remains the removal of the Peer or the Remote.
- A later convenience by which logged-in machines find each other must still end in an Invite's redemption or something as strong. One that pairs on the Relay's word reopens this decision.
- A machine reached through a Relay is usually one nobody is sitting at, so keeping its login current cannot depend on someone being there. ADR-0048 decides how that is done.
