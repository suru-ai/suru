# Window a Remote's Session where it lives, and name other Servers where it is read

A Sidekick's reading of a Session is taken by the Server holding that Session, and only the slice the reading asks for crosses a Pairing. A Remote's Session API answers `GET /v1/sessions/{session_id}/reading` with the Session's summary and an excerpt: its Turns windowed, its entries numbered and capped, the point to read on from, and every word chosen — but for the other Servers it names, which stand as typed references. The Sidekick's own Server renders those, naming a Peer's Sidekick or a Subsession's Server as it reaches them, as it already did for a whole Session it fetched. So a Remote's Session reads however large it grows, and ADR 0044's rule that a Sidekick does on a Remote only what its user's Client could still holds: the route is the Remote's own Session API, carried by the same Pairing, and it reads nothing a Client could not.

The asker says how much of an answer it reads, and the Remote refuses an answer past that rather than send it, so the Sidekick is told to ask for less. A narrower read always fits but for one entry larger than the budget on its own, which Suru's own storage caps keep far from the default; that entry is read on the Remote.

## Considered Options

- **Fetch the whole Session and window it on the reader, as before.** Rejected: a Session past what the reader takes of one answer could not be read from there at all, and every read moved the whole Session to show a few thousand characters.
- **Have the Remote render the reading whole, the reader sending its own Pairings.** Rejected: it hands every Remote read the names and keys of the reader's other Remotes, against the direction a Pairing trusts, and has one Server speak in another's names.
- **Send a pruned snapshot and project it on the reader.** Rejected: numbering, a Turn's heading, the cap's cut and the final Message all need whole Turns, so pruning would mean placeholder entries and a second copy of the window kept in step with the first.
- **Page the Session API by Turn ranges.** Rejected: a window is capped by characters counted back from its end, so the reader cannot know which Turns it needs without their content, and one Turn may be huge.
- **Cap the answer's bytes inside the projection, cutting the read short where it would not fit.** Rejected for now: it adds a second cap to the projection's most intricate loop for a read that, at the default budget, asks for millions of characters at once.

## Consequences

- How a reading numbers a Session's entries is part of what Servers say to each other: a point one Server gives, another passes back to it, so changing the numbering changes the protocol version.
- What a reading names on another Server must stay a typed reference in the excerpt; a phrase that resolves one inside the projection would name Servers as the Remote knows them.
- A read that finds the Session while an act of the reader's Sidekick on it is not yet confirmed reads the outline of the Session's tree to judge that act, since the excerpt carries nothing to judge it by.
- A Remote on another protocol version is refused, as any Pairing across versions is (ADR 0017), so no reader falls back to the whole fetch.
