# Begin a Turn of its own for a requested Compaction

A Compaction is an Activity, and every Activity belongs to a Turn, but a Compaction the user asks for with `/compact` arrives while the Session is idle, when there is no Turn to hold it. We decided that the request itself begins a Turn: one with no user Message, whose only content is the Compaction, and which Settles as the Compaction does. This is what Codex and Claude do natively — each runs a manual compaction as a turn with its own start and end — so the Turn still Settles at the Provider's boundary (ADR 0015), and the Session is Working, interruptible (ADR 0039), and accountable for Usage and Cost while it runs, by the rules every other Turn already follows. An automatic Compaction needs none of this: it stands in whatever Turn it fell in.

Because Suru alone knows which Turns it began this way, whether a Compaction was manual or automatic is read from the Turn that holds it rather than from the Provider, which Codex does not say.

## Considered Options

Admitting `/compact` as a Prompt was rejected, although it is the least new machinery and literally how Claude is driven: it would draw a Suru command in the Transcript as something the user said to the Agent, and a Prompt is steerable where a Compaction is not. Letting a Compaction stand between Turns as a turnless Activity was rejected because it breaks an invariant every other Activity, the Turn Fold, and Settle rely on, and leaves nothing to carry Working, interruption, or Usage.

## Consequences

A Turn can now have no Message at all, so anything that assumes a Turn opens on a user Message or a Delegation must allow for one that does not.

Such a Turn accepts no steer: a Prompt sent to steer while it runs is held, admitted and owed the next Turn, and the Session is Working throughout. Suru holds it itself rather than leaning on any Provider's own queue. What becomes of it follows how the Turn settles, which is how its Compaction did: a Turn that completes — including one whose Compaction completed before an interrupt landed — lets the held Prompt begin the next Turn on the compacted context; a Turn that settles any other way, failed or interrupted, withdraws it in the commit that settles the Turn, so the Session stops Working with the Turn and the Prompt never reaches a context its writer expected compacted. Its text returns to the composer of the client that wrote it, as a draft that is sent again as a new Prompt — not, as ADR 0024 has it for an interrupted Prompt, to the client that interrupted — because an interrupt here stopped the Compaction rather than asking for the Prompt back, and a failure has no interrupter at all.

A queued Prompt is not held. It waits in the queue as it would behind any Turn, and is delivered once the Turn settles however it ended, because queueing says "after this" rather than "after this is compacted". For the same reason a queued Prompt cannot be promoted to steer while the Compaction runs: there is nothing to steer, so the Session refuses the promotion and the Prompt stays queued.
