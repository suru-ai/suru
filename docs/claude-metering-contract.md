# Claude metering contract

Verified against the suggested Claude Code CLI **2.1.237** while implementing issue [#324](https://github.com/jake-tucker/suru/issues/324). The published `@anthropic-ai/claude-code-linux-x64@2.1.237` binary ran in stream-json input/output mode against a local HTTP fixture serving deterministic Anthropic message streams. These measurements exercise the real CLI's accounting with synthetic API usage; they do not measure billing.

## Successive results and process lifetimes

Two user messages were sent sequentially through one running CLI process. The fixture supplied uncached Haiku input/output counts, with no tool calls:

| API response | Input tokens | Output tokens | CLI `result.usage` input/output | CLI `total_cost_usd` |
| --- | ---: | ---: | --- | ---: |
| First | 100 | 5 | 100 / 5 | 0.000125 |
| Second | 200 | 7 | 200 / 7 | 0.000360 |

The second `modelUsage` carried 300 input and 12 output tokens. The dollar total is cumulative within the running process; adding both result totals would count the first call twice. A fresh process launched with `--resume` for the same native Session ID reported 0.000125 and then 0.000360 for the same synthetic responses: the resumed conversation does not restore the previous process's metering total.

Sending `/clear` in this tested stream-json environment returned an unavailable-command result with zero Turn usage and the unchanged cumulative Cost. It did not establish a new accounting lifetime. Current documentation describes reset messages in environments that support these commands; a repeated or lower amount alone must not be mistaken for proof of a reset.

## Descendants and Model identity

The fixture made the first parent response invoke `Agent`, targeting a custom Subagent configured with a different Model. The CLI emitted `task_started` with the spawning `tool_use_id`, followed by a child assistant snapshot carrying that identity as `parent_tool_use_id` and the actual child Model in `message.model`.

| Work | Model | Input tokens | Output tokens | CLI model Cost |
| --- | --- | ---: | ---: | ---: |
| Parent, two API responses | claude-haiku-4-5-20251001 | 400 | 12 | 0.000460 |
| Child, one API response | claude-sonnet-5 | 200 | 7 | 0.000705 |

The result's `usage` contained only the parent's 400 input and 12 output tokens. Its `total_cost_usd` was **0.001165**, including the child, and `modelUsage` contained both Models. Both parent and child assistant snapshots carried `output_tokens: 1`, the message-start placeholder, despite the final API stream reporting larger counts. Such snapshots establish Model identity but cannot establish complete child Usage or Cost.

The observed CLI tool was named `Agent`; older Suru fixtures used `Task`. The task lifecycle's `tool_use_id` is the stable attribution link, not the tool's display name.

## Source and limits

These observations agree with the official [streaming-input cost documentation](https://code.claude.com/docs/en/agent-sdk/cost-tracking#track-costs-in-streaming-input-mode) and its discussion of [output-token placeholders](https://code.claude.com/docs/en/agent-sdk/cost-tracking#read-output-tokens-from-the-result-message). A direct inference probe was blocked by the account's session limit; the deterministic local fixture allowed the pinned CLI's metering behavior to be verified without inference. Other CLI versions and supported reset notifications remain separate contracts to validate when the suggested version changes.
