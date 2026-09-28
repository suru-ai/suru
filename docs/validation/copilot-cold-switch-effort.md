# Copilot refuses a reasoning effort on a cold Model switch

Recorded 2026-09-28 against GitHub Copilot CLI 1.0.88, driven through the Rust SDK by
`examples/copilot_effort_probe.rs`.

## The symptom

A brokered Subagent spawned on Copilot's `gpt-6-luna` with reasoning effort `none` — the default
`list_providers` showed for it — failed at once:

```
Provider execution failed: Copilot Model selection failed: RPC error -32603: Request
session.model.switchTo failed with message: Reasoning effort 'none' is not supported for model
'gpt-6-luna'.
```

Suru's catalog had `none` from the CLI itself: `models.list` names it first among the Model's
`supportedReasoningEfforts`, and names no `defaultReasoningEffort` for any Model, so Suru's
first-listed-choice fallback made it the default.

## What the CLI does

Every switch below is `session.model.switchTo` with `reasoningEffort` set, on a Session created
with no Model, as Suru opens every Copilot Session before switching it onto the Agent Selection.

| Session before the switch | `none` | `low` | no effort |
| --- | --- | --- | --- |
| fresh | refused | accepted | accepted |
| fresh, after a switch onto the Model with `low` | refused | | |
| fresh, after a switch onto the Model with no effort | refused | | |
| created on `gpt-5-mini` | refused | | |
| created on `gpt-6-luna` itself | refused | | |
| fresh, 3 s later | refused | | |
| fresh, after `session.model.list` on that Session | **accepted** | | |

`PROBE_ALL` switched a fresh Session onto every Model the CLI lists with `none`: refused on all
thirteen, whether or not the Model's own listing offers `none`. `PROBE_ISO` isolated the one thing
that changes the verdict: asking the CLI for that Session's own catalog first. Asking on another
Session does not carry over. `PROBE_TURN` then ran a Turn on `gpt-6-luna` at `none` after listing:
the switch was accepted, `session.model.getCurrent` reported the effort in force, and the Turn
completed. Listing took 4 ms.

`session.model.list` carries `capabilities.supports.reasoning_effort` per Model, `none` included
for `gpt-6-luna`; the SDK documents that connected hosts should call it. The reading taken here is
that the CLI validates a switch against the catalog it has resolved for the Session, and resolves
that catalog only on request, falling back to a stricter source before then. A `session.create`
naming the Model and `none` up front is accepted, so an Errand, which opens its Session that way, was
never affected.

## What Suru does about it

A Copilot Session lists its own Models once, before its first Model switch, and discards the
answer (`CopilotSession::apply_selection`). The scripted CLI in `tests/copilot_integration` has a
`cold_switch_refusing_arm` that judges a switch the way 1.0.88 does, and
`a_switch_naming_a_reasoning_effort_is_made_only_after_the_sessions_models_are_listed` fails
without the listing.

The parent Agent, meanwhile, had read `status: failed` with no message, because the Subagent's
error row is not a Message: `read_subagent` and the Subagent Report now carry what a failed
stretch failed with.

## Left open

Copilot names no default effort for any Model, so Suru's default is the first choice the CLI
lists, which is `none` — reasoning off — on the GPT 5.4, 5.6 and 6 families. That is a valid
choice now, but it is Suru's guess, not Copilot's: the CLI itself sends no effort when none is
chosen and lets the service decide.
