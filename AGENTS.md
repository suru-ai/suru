You have access to the following codebases under ./references/ you should use for inspiration
- t3-code - The main inspiration for the core provider setup & server architecture
- opencode - Use as a reference for how the TUI should look and function
- codex - OpenAI's CLI application for using thier models, use when building our Codex integration
- copilot-sdk - GitHub Copilot SDK. Use for building the Copilot integrations

## Development Notes
- To start we will only support the Codex and Copilot providers, but additional providers may be added at a later date.
- Build all functionality supporting both providers to avoid building interfaces that are not generic enough to
support others down the line.
- This application is very early in development. Freely make breaking changes if they result in better code.
- Do not account for backwards compatibility with previous versions.
- Build using interfaces designed for an eventual plugin architecture based on that of OpenCode.
- User-facing configuration has a surface: Settings are declared in the compile-time schema in `src/settings.rs`, pinned by Config Documents under the config root, and edited from the settings panel. Each Setting also declares the group whose tab of that panel presents it. Promoting a value to a Setting is a schema entry, not new machinery — but promote deliberately.
- Run tests with `cargo nextest run` (parallelizes across test binaries and reports per-test timings); `cargo test` also works. Tests must not wait out production-scale delays: timing constants (timeouts, backoff, keepalive) are injectable via builders such as `ManagedClientConfig::with_startup_timeout`, `ServerTimings`, and `CodexRuntime::with_interrupt_request_timeout`, so inject millisecond-scale values instead of sleeping.

## Plugins

Use OpenCode's TUI plugin architecture as the reference when designing core UI extension seams. For now, keep named
render slots crate-private with typed context, built-in defaults, deterministic prepend/replace/append composition, and
failure isolation; slots contribute rendered content only, while commands and Session mutation use separate typed
interfaces. Keep transcript projection typed rather than adding a generic row slot so future tool renderers can target
understood Activity types. A plugin loader and public plugin API are deferred, but visual actions should use semantic
command IDs so future plugins and mouse input can invoke the same behavior.

## Agent skills

Skills are present under the .agents/skills directory. Read from there if a skill trigger fails.

### Issue tracker

Issues and specs are tracked in GitHub Issues. See `docs/agents/issue-tracker.md`.

### Triage labels

Triage uses the five canonical label names. See `docs/agents/triage-labels.md`.

### Domain docs

Domain documentation uses a single-context layout. See `docs/agents/domain.md`.
