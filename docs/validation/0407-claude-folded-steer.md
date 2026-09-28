# How Claude folds a queued message into its running loop (#407)

Captured 2026-09-28 against Claude Code **2.1.283** (`/usr/bin/claude`, a
wrapper that sets `DISABLE_UPDATES=1` and runs `/opt/claude-code/bin/claude`,
signed in with a claude.ai account). The Model was `haiku`
(`claude-haiku-4-5-20251001`). Each case launched one stream-json process, as
Suru's Claude Provider does, from an empty scratch directory:

```
claude -p --input-format stream-json --output-format stream-json --verbose
  --replay-user-messages --include-partial-messages --model haiku
  --setting-sources "" --permission-mode default
  --allowedTools 'Bash(sleep:*)' 'Bash(sleep *)' --session-id <uuid>
```

**Environment:** every inherited `CLAUDE*` variable was removed first, as for
`0421-broker-smoke.md`.

**Driver:** a throwaway Python script, not committed. It wrote each user message
to stdin in Suru's envelope, plus a fresh v4 `uuid`:

```json
{"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "…"}]},
 "parent_tool_use_id": null, "session_id": "", "uuid": "<v4>"}
```

It stamped every stdout line with the time since launch, and wrote the second
message on a trigger. It closed stdin once 25 s had passed without output after
a `result`.

**Messages:** the first asked for one Bash `sleep 20`, then the one-word reply
`DONE`. Cases C, D and E varied it, as their rows say. The second message was
always `Also say the word MARK.`

**Variants:** two variants of case A dropped `--replay-user-messages`, or the
`uuid`.

**Cost:** the eight runs cost $0.15 in all.

## Findings

| Case | Second message written | `result`s | Answer | Second message taken up |
| --- | --- | --- | --- | --- |
| A | 5 s into the `sleep 20` call | 1 (`num_turns: 2`) | `DONE MARK` | At the tool round: `started` 3 ms after the `tool_result`, before the next request |
| A, no `--replay-user-messages` | 5 s into the call | 1 (`num_turns: 2`) | `DONE MARK` | As in A: `command_lifecycle` does not need the flag |
| A, no `uuid` | 5 s into the call | 1 (`num_turns: 2`) | `DONE`, `MARK` | As in A, but nothing on stdout names it: no `command_lifecycle`, and the replay carries a uuid of the CLI's own |
| B | 1 s after the first `result`, loop at rest | 2 (`num_turns: 2`, then `1`) | `DONE.`, then `MARK.` | At once, as a loop of its own with its own `system` `init` |
| C | 2 s into the first of two sequential `sleep 5` calls | 1 (`num_turns: 3`) | `DONE MARK` | At the first tool round. The loop then made the second call. |
| D | On the first text delta of the loop's last request (`sleep 3`, then a long list) | 2 (`num_turns: 2`, then `1`) | The list without MARK, then `MARK` | After the first `result`, as a loop of its own |
| E | Two messages, 3 s and 6 s into the call | 1 (`num_turns: 2`) | `DONE` | Both at the same tool round. The on-disk transcript holds two `queued_command` attachments; the Model ignored both. |
| F | 3 s into the call, then an `interrupt` with `cancel_queued: true` at 6 s | 1, `error_during_execution`, `aborted_tools` | None | Never: `cancelled` |

### The rule Claude follows

A user message written while a loop runs is queued. At each tool round, once
that round's tool results are in and before the loop's next model request, the
loop takes up everything queued. It sends it with that request as a
`queued_command` attachment, and the loop's one `result` answers it with the
rest.

A message still queued when the loop ends, because its last response called no
tool, begins a loop of its own once that `result` is out. The new loop has its
own `system` `init` and its own `result`. A message written to a process at
rest begins a loop at once.

So #129's rule is only half true: "the CLI answers every user message queued
into a running loop with a `result` of its own" holds only for a message queued
after the loop's last tool round (case D, and #129's own capture against
2.1.237, whose steer landed while the answer streamed). A message queued while
a tool runs gets no `result` of its own (cases A, C, E). That is the #421 wait
smoke's case: the Report arrives in the instant `wait_subagents` answers. A
user's steer Prompt written while a tool call runs is folded the same way:
case A is exactly that message.

### The signal

When a user message carries a `uuid`, the CLI reports that message's fate on
stdout under it:

```json
{"type": "command_lifecycle", "command_uuid": "<the message's uuid>",
 "state": "queued" | "started" | "completed" | "cancelled" | …, "uuid": "…", "session_id": "…"}
```

Case A, in order (times since launch):

```
 0.620  command_lifecycle  queued     <Prompt>
 0.621  command_lifecycle  started    <Prompt>
 0.664  system init
 3.091  assistant  tool_use Bash {"command": "sleep 20"}
 8.091  >>> stdin: the second message
 8.092  command_lifecycle  queued     <steer>
23.769  user  tool_result
23.773  user  isReplay: true, uuid <steer>        (only with --replay-user-messages)
23.774  command_lifecycle  started    <steer>
23.774  system status requesting
24.868  stream_event message_start
26.412  assistant  text "DONE MARK"
26.420  command_lifecycle  completed  <steer>
26.422  result  success, num_turns 2, "DONE MARK"
26.423  command_lifecycle  completed  <Prompt>
```

Case D:

```
17.108  >>> stdin: the second message, while the loop's last request streams
17.588  stream_event message_stop (end_turn)
18.063  command_lifecycle  queued     <steer>
18.070  result  success, num_turns 2
18.402  command_lifecycle  completed  <Prompt>
18.403  command_lifecycle  started    <steer>
18.534  system init
19.500  result  success, num_turns 1, "MARK"
19.501  command_lifecycle  completed  <steer>
```

- **`queued`** comes as the CLI reads the message. Case D shows it can come
  late: the CLI read the message about a second after it was written, though
  still before the `result`.
- **`started`** comes when a loop takes the message up, whether folded in at a
  tool round (A, C, E) or beginning a loop of its own (B, D).
- **`completed`** comes before the `result` of a loop the message was folded
  into, and after the `result` of a loop the message began.
- **`cancelled`** covers case F. The interrupt drops the queued steer before
  the interrupt's `control_response`, and the receipt now lists it:
  `{"still_queued": [], "cancelled": ["<steer>"]}`. The Prompt whose loop the
  interrupt aborted is `cancelled` after the aborted `result`.

At a `result`, then, every message a loop has `started` is answered. A message
not yet `started` will begin a loop of its own.

**Sent without a `uuid`,** no `command_lifecycle` comes at all. That is what
Suru sent until now, and why #421's run 5 saw nothing.

**`--replay-user-messages`** echoes each message as a `user` line with
`isReplay: true`, at the moment of `started`. It carries the client's `uuid`
when one was sent, and a fresh one when not. It gives no terminal states.

**The on-disk transcript** agrees with the stream. In case A:

1. `queue-operation enqueue` at the moment of the write.
2. After the `tool_result`, a `queued_command` attachment whose `source_uuid`
   is the steer's `uuid`.
3. `queue-operation remove`.

**The CLI's own schema** for the frame is marked `@internal`, and is emitted
"on the stdout stream in -p/SDK sessions". It says:

- 'started' when it drains into a turn.
- A command that starts a fresh turn emits 'completed' AFTER that turn's result
  frame … a command folded into an already-in-flight turn emits 'completed'
  BEFORE that turn's result frame.
- Commands enqueued without a uuid … emit no lifecycle events.

The same schema, with those sentences, is in the 2.1.280 binary (the npm
`@anthropic-ai/claude-code-linux-x64@2.1.280` package), the version Suru
suggests.

## Decision

**Suru writes every Prompt and steer with a fresh v4 `uuid`, and reads
`command_lifecycle`.** It does not adopt `--replay-user-messages`. The frame
gives the same moment without a flag, and adds the terminal states
(`src/provider/claude/turn_in_flight.rs`).

- **Settling:** a successful `result` Settles the Turn unless a message written
  into it has not yet been `started`. Such a message begins a loop of its own,
  that loop runs inside the Turn the message steered, and its `result` Settles
  the Turn.
  - A folded steer (cases A, C, E) is `started` before the `result`, so the one
    `result` Settles the Turn.
  - A steer the loop ended without taking up (case D) keeps the Turn running
    for its own loop.
  - A Prompt written while the CLI runs a loop of its own, waking for a
    background task, is not answered by that loop's `result` either.
- **A message that will never run:** a message not yet `started` may end
  instead, `cancelled`, `discarded` or `refused`, after the `result` of the
  Turn's last loop has left the Turn waiting on it. Then the Turn Settles
  Completed at that frame, since nothing else will Settle it.
- **A CLI that reports no lifecycle:** one older than the frame, or a later one
  that drops it, gives no account. There every successful `result` Settles the
  Turn. A loop that a still-queued steer begins afterwards opens a native
  Continuation at its `message_start`. A Turn therefore never waits on a
  `result` a folded steer will not get, and nothing a later loop writes is
  dropped. This is what makes relying on an internal frame safe: losing it
  costs the grouping of a case D steer, not a hung Session.
- **Interrupts are unchanged.** The aborted `result` settles the Turn, and the
  `cancelled` frames that follow name messages Suru has stopped waiting on.

The scripted suite models both outcomes:

- The stand-in now reads each message's `uuid` and can report its lifecycle.
- `tests/claude_integration/steering.rs` pins cases A and D, and both again
  from a CLI that reports no lifecycle.
- `tests/claude_integration/broker.rs` pins a Report folded into a Turn
  waiting in `wait_subagents`.
- `tests/claude_integration/turns.rs` pins case B.

The #421 wait smoke now waits past the answer for the Turn that waited to
settle (`0421-broker-smoke.md`).

Not verified live:

- **A steer that ends untaken after the `result`** (`cancelled`, `discarded`
  or `refused` with the Turn waiting). No capture produced one; it is pinned
  only in unit tests.
- **Versions before 2.1.280.** When the frame first shipped was not checked.
- **A user's steer Prompt through Suru end to end.** It was not driven against
  the live CLI; case A drove the same message against the raw CLI.
