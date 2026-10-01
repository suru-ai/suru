# Begin a Turn of its own for a requested Compaction

A Compaction is an Activity, and every Activity belongs to a Turn, but a Compaction the user asks for with `/compact` arrives while the Session is idle, when there is no Turn to hold it. We decided that the request itself begins a Turn: one with no user Message, whose only content is the Compaction, and which Settles as the Compaction does. This is what Codex and Claude do natively — each runs a manual compaction as a turn with its own start and end — so the Turn still Settles at the Provider's boundary (ADR 0015), and the Session is Working, interruptible (ADR 0039), and accountable for Usage and Cost while it runs, by the rules every other Turn already follows. An automatic Compaction needs none of this: it stands in whatever Turn it fell in.

Because Suru alone knows which Turns it began this way, whether a Compaction was manual or automatic is read from the Turn that holds it rather than from the Provider, which Codex does not say.

## Considered Options

Admitting `/compact` as a Prompt was rejected, although it is the least new machinery and literally how Claude is driven: it would draw a Suru command in the Transcript as something the user said to the Agent, and a Prompt is steerable where a Compaction is not. Letting a Compaction stand between Turns as a turnless Activity was rejected because it breaks an invariant every other Activity, the Turn Fold, and Settle rely on, and leaves nothing to carry Working, interruption, or Usage.

## Consequences

A Turn can now have no Message at all, so anything that assumes a Turn opens on a user Message or a Delegation must allow for one that does not. Such a Turn accepts no steer: a Prompt admitted while it runs is held to begin the next Turn, and withdrawn if the Compaction fails or is interrupted.
