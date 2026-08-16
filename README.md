# Chidori

Chidori is an agent harness heavily drawing inspiration from tools like OpenCode and T3 Code.

The goal is to provide a high-performance, configurable, and provider-agnostic agentic coding experience,
initially running as a TUI, but potentially eventually with native GUIs across the various platforms.

## Persistent-server smoke check

1. Build Chidori with `cargo build`.
2. Run `target/debug/chidori` in two terminals.
3. Confirm both TUIs show the same server ID and PID, and that their counters advance together.
4. Press `q` in both TUIs, wait a few seconds, then run `target/debug/chidori` again.
5. Confirm the relaunched TUI shows the same server ID and PID with a counter value that advanced while no TUI was connected.

The detached debug server intentionally remains running after this check. Its output is written to the debug channel's `server.log` under Chidori's per-user state directory.
