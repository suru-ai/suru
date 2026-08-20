# Suru

Suru is an agent harness heavily drawing inspiration from tools like OpenCode and T3 Code.

The goal is to provide a high-performance, configurable, and provider-agnostic agentic coding experience,
initially running as a TUI, but potentially eventually with native GUIs across the various platforms.

## Persistent-server smoke check

1. Build Suru with `cargo build`.
2. Run `target/debug/suru` in two terminals.
3. Confirm both TUIs show the same server ID and PID.
4. Press `Ctrl+C` with an empty composer in both TUIs, wait a few seconds, then run `target/debug/suru` again.
5. Confirm the relaunched TUI shows the same server ID and PID, proving the shared server survived without connected clients.

The detached debug server intentionally remains running after this check. Its output is written to the debug channel's `server.log` under Suru's per-user state directory.
