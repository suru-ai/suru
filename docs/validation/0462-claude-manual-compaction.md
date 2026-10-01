# What Claude writes for `/compact` sent over stream-json (#462)

Captured 2026-10-01 against Claude Code **2.1.283** (Homebrew cask, signed in
with a claude.ai account). The Model was `haiku` (`claude-haiku-4-5-20251001`).
Each case launched one stream-json process from an empty scratch directory:

```
claude --print --input-format stream-json --output-format stream-json --verbose
  --strict-mcp-config --no-session-persistence --setting-sources "" --model haiku
```

**Driver:** a shell pipeline, not committed. It wrote each user message to stdin
in Suru's envelope, and waited long enough for the `result` before closing
stdin.

**Cases:**

- **Refusal:** `/compact` as the first message, with a `uuid` and
  `--include-partial-messages`, so there was nothing to compact.
- **Completed:** `--tools ""` and no partial messages. The first message was
  `Reply with just the word OK.`, then `/compact` once its `result` was out.

**Cost:** the refusal cost nothing. The completed case cost $0.016, of which
the compaction was $0.007.

## Findings

### Refusal: nothing to compact

The CLI reported the message `queued` and `started` under its `uuid`, then wrote
its `system/init`. There was no `system/status` at all. What followed was a
synthetic `assistant` message carrying the command's error output and its
outcome:

```json
{"type": "assistant",
 "message": {"model": "<synthetic>", "role": "assistant", "stop_reason": "end_turn",
             "usage": {"input_tokens": 0, "output_tokens": 0, …},
             "content": [{"type": "text", "text": "Error: No messages to compact"}], …},
 "parent_tool_use_id": null,
 "local_command_source": "<local-command-stderr>Error: No messages to compact</local-command-stderr>",
 "local_command_run": {"command": "compact", "args": ""},
 "local_command_outcome": {"kind": "failed"}, …}
```

Then came a `result` reading `subtype: "success"`, `is_error: false`,
`num_turns: 0`, `result: ""`, `local_command: "compact"`, with zero usage and
`total_cost_usd` unchanged. The message's lifecycle then ended `completed`.

### Completed

```
system/status        {"status": "compacting"}
system/status        {"status": null, "compact_result": "success"}
system/init
system/compact_boundary
                     {"compact_metadata": {"trigger": "manual", "pre_tokens": 4602,
                      "post_tokens": 707, "cumulative_dropped_tokens": 3895,
                      "duration_ms": 12045}, "logical_parent_uuid": …}
user                 {"isSynthetic": true, "isReplay": false,
                      "message": {"content": "This session is being continued from a previous
                      conversation …"}}
user                 {"isReplay": true,
                      "message": {"content": "<local-command-stdout>Compacted </local-command-stdout>"}}
result               {"subtype": "success", "num_turns": 0, "result": "",
                      "local_command": "compact", "usage": {all zero},
                      "total_cost_usd": 0.0162725, "modelUsage": {…}}
```

The success path wrote no synthetic `assistant` message. The boundary carried
no `preserved_segment` in this case.

### What Suru reads from this

- The closing `result` reads success with zero usage whether the CLI compacted
  or refused, so it does not say how the compaction went. The Turn a Compaction
  request began Settles as its Compaction did. The Compaction Settles from
  `status`, `compact_boundary`, and the synthetic message's
  `local_command_outcome`. A refusal reports nothing else.
- `result.usage` meters only the loop's own model calls, and a `/compact` loop
  makes none, so its zeros state no token count. `total_cost_usd` stays
  cumulative, as `docs/claude-metering-contract.md` records, and it rose by
  the compaction's own cost ($0.0069 here). The Turn records that running
  total as every Claude Turn does, so the Session's Cost counts the rise once.
- The synthetic `assistant` message (`local_command_source`) and the
  `isReplay` `<local-command-stdout>` message are the CLI's plumbing. Neither
  is an Agent Message or a user Message.
- The CLI reports a `/compact` sent with a `uuid` through the same message
  lifecycle as a Prompt, `started` before its `result`. So that `result`
  Settles the Turn like any other.
