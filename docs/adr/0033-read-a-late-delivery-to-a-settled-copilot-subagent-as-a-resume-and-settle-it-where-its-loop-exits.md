# Read a late delivery to a settled Copilot Subagent as a resume, and settle it where its loop exits

Copilot's `write_agent` never steered a working Subagent in the CLIs captured (1.0.87, 1.0.88): a message sent mid-stretch is queued and delivered as `delivery: "queued"` only after that stretch's `subagent.completed`, and one sent to an agent that has already finished arrives as `delivery: "idle"`. Either way the same agent runs again on its own context, with no second `subagent.started` and nothing to close the new run (`docs/validation/0397-copilot-subagent-steer.md`, `0398-copilot-subagent-resume.md`). By the glossary's Delegation entry that is a resume, so Copilot joins ADR 0031: a `user.message` addressed to a Subagent whose stretch has settled begins a resume Turn in its Session, whatever its `delivery` says, and the spawn's own `idle` prompt is told apart because it arrives before the Subagent has worked. The `resumable: false` that `subagent.started` carries is typed by neither pinned SDK and is ignored. Because Copilot closes the resumed run with nothing, Suru settles it where the agent loop exits: at the Subagent's `assistant.turn_end` that follows a model message requesting no tools, with the next Delegation to that Subagent and the loop's `session.idle` as backstops, and an interrupt's aborted idle settling it interrupted, as it does a spawn.

## Considered Options

- **Settle on the `agent_idle` system notification.** Rejected: it named the resumed agent in one of three captured resumes and never fired in the other two.
- **Settle on `session.idle` alone.** Rejected: the row would show Working, and its duration keep counting, for as long as the parent went on working after the agent had answered, every `read_agent` round of it.
- **Read `queued` as a steer of the stretch it was sent during.** Rejected: the message never reached that stretch, so placing it there would stand it before output produced without it (ADR 0032).
- **Keep dropping it.** Rejected: the Subagent's second stretch was lost entirely, and the parent's `read_agent` then quoted a report Suru never showed.

## Consequences

- Each `user.message` a settled Subagent consumes is its own resume. Copilot fires the event on consumption, so its arrival is also proof the previous run ended, and it settles any resume Turn of that Subagent still open.
- A resume a sibling delegated stands in the sibling's Transcript, in a Continuation of the sibling's Session where the sibling has settled; CONTEXT.md's Subagent entry now says so for every Provider.
- Copilot still offers no stop of one Subagent, so a resumed one is stopped with the whole loop, as a spawned one is.
- The Subagent's Model carries into the resumed Turn; `session.model_change` fires only at spawn.
- The `steering` path ADR 0032 describes stays as documented and unobserved.
