# Claude metering contract

## Claude Code CLI 2.1.280

Re-verified on 2026-09-27 against the installed `2.1.280 (Claude Code)`, the suggested version, with live Haiku Turns in stream-json mode and the Agent SDK 0.3.280 type definitions. Where this section and the 2.1.237 record below disagree, this section is current.

- **A resumed conversation continues its running total.** One process ran two Turns (`total_cost_usd` 0.0121764, then 0.0160821) and exited when stdin closed. A fresh process launched with `--resume` for the same native Session ID reported **0.0192634** on its first Turn, and its `modelUsage` carried every earlier Turn's tokens as well. The 2.1.277 changelog describes the change ("headless sessions now save their totals at exit"), and the SDK's `total_cost_usd` documentation now reads "a resumed or forked session continues from the total its transcript saved". The `result_index` field restarts at 0 in each process.
- **The total is saved only when the process exits cleanly.** A process that ran one Turn (0.0218079) and was then killed with `SIGKILL` saved nothing. The next resume continued from the previous clean exit's 0.0192634, so its first result (0.0218171) lacked the killed process's spend. Suru stops a child by closing its stdin and forces it down only after the exit grace, so the normal path saves the total.
- **`/clear` now works in stream-json and begins a new conversation.** Sent as a user message, it produced a `conversation_reset` message, a `system/init` naming a different `session_id`, and a zero-usage result whose `total_cost_usd` was **0** under that new `session_id`.
- **Thinking is broken out of output.** `result.usage.output_tokens` counts thinking, and `result.usage.output_tokens_details.thinking_tokens` states how much of it was thinking (for example, 46 output tokens with 39 of thinking). `modelUsage[*].thinkingTokens` carries the cumulative figure.
- **Unchanged from 2.1.237:** `result.usage` covers only the loop's own API calls for that result, while `total_cost_usd` and `modelUsage` are cumulative and include Subagents. A spawned Subagent's (Haiku) tokens appeared only in `modelUsage` and the dollar total. Assistant snapshots still carry placeholder `output_tokens` (6 in this capture) and only establish Model identity.

Suru reads these as follows. A Claude result's cumulative Cost is reported in the reporting lifetime `claude:<session_id>`, keyed by the conversation the result names. That lifetime runs through every process that resumes the conversation, including processes started for a Selection change or after a Suru restart, so earlier spend is never counted twice. A `/clear` starts a new lifetime from zero. A total that goes back within one lifetime, such as after a killed process, is not believed: the Turn's Cost is left partial rather than lowering the recorded total. Result thinking tokens become Usage `reasoning_tokens` and are subtracted from `output_tokens`. Suru's Resume State still names the conversation the Session was spawned under, so a `/clear` followed by a respawn resumes the pre-clear conversation. That is a known gap.

## Claude Code CLI 2.1.237

Verified against the then-suggested Claude Code CLI **2.1.237** while implementing issue [#324](https://github.com/suru-ai/suru/issues/324). The published `@anthropic-ai/claude-code-linux-x64@2.1.237` binary ran in stream-json input/output mode against a local HTTP fixture serving deterministic Anthropic message streams. These measurements exercise the real CLI's accounting with synthetic API usage; they do not measure billing.

### Successive results and process lifetimes

Two user messages were sent sequentially through one running CLI process. The fixture supplied uncached Haiku input/output counts, with no tool calls:

| API response | Input tokens | Output tokens | CLI `result.usage` input/output | CLI `total_cost_usd` |
| --- | ---: | ---: | --- | ---: |
| First | 100 | 5 | 100 / 5 | 0.000125 |
| Second | 200 | 7 | 200 / 7 | 0.000360 |

The second `modelUsage` carried 300 input and 12 output tokens. The dollar total is cumulative within the running process; adding both result totals would count the first call twice. A fresh process launched with `--resume` for the same native Session ID reported 0.000125 and then 0.000360 for the same synthetic responses: the resumed conversation does not restore the previous process's metering total.

Sending `/clear` in this tested stream-json environment returned an unavailable-command result with zero Turn usage and the unchanged cumulative Cost. It did not establish a new accounting lifetime. Current documentation describes reset messages in environments that support these commands; a repeated or lower amount alone must not be mistaken for proof of a reset.

### Descendants and Model identity

The fixture made the first parent response invoke `Agent`, targeting a custom Subagent configured with a different Model. The CLI emitted `task_started` with the spawning `tool_use_id`, followed by a child assistant snapshot carrying that identity as `parent_tool_use_id` and the actual child Model in `message.model`.

| Work | Model | Input tokens | Output tokens | CLI model Cost |
| --- | --- | ---: | ---: | ---: |
| Parent, two API responses | claude-haiku-4-5-20251001 | 400 | 12 | 0.000460 |
| Child, one API response | claude-sonnet-5 | 200 | 7 | 0.000705 |

The result's `usage` contained only the parent's 400 input and 12 output tokens. Its `total_cost_usd` was **0.001165**, including the child, and `modelUsage` contained both Models. Both parent and child assistant snapshots carried `output_tokens: 1`, the message-start placeholder, despite the final API stream reporting larger counts. Such snapshots establish Model identity but cannot establish complete child Usage or Cost.

The observed CLI tool was named `Agent`; older Suru fixtures used `Task`. The task lifecycle's `tool_use_id` is the stable attribution link, not the tool's display name.

### Source and limits

These observations agree with the official [streaming-input cost documentation](https://code.claude.com/docs/en/agent-sdk/cost-tracking#track-costs-in-streaming-input-mode) and its discussion of [output-token placeholders](https://code.claude.com/docs/en/agent-sdk/cost-tracking#read-output-tokens-from-the-result-message). A direct inference probe was blocked by the account's session limit; the deterministic local fixture allowed the pinned CLI's metering behavior to be verified without inference. Other CLI versions and supported reset notifications remain separate contracts to validate when the suggested version changes.
