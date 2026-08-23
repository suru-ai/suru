# Drive Claude over stream-json, not the Agent SDK

The Claude Provider speaks the Claude Code CLI's stream-json protocol directly — a long-lived `claude` child per Session with control requests for interrupt, permissions, and model listing — rather than embedding Anthropic's official Claude Agent SDK, which is a Node library and would put a Node runtime inside a Rust host. This keeps the Provider in the same shape as Codex (ADR 0004: a hand-rolled wire layer against a native harness), using the SDK's source and t3's adapter as protocol documentation instead of dependencies.

## Consequences

The stream-json control protocol is an SDK implementation detail, not a documented public surface, so protocol drift is ours to absorb: the Provider pins a minimum CLI version, verified at the availability probe and surfaced as `IncompatibleVersion`, and the floor moves only when we have tested against the newer wire.
