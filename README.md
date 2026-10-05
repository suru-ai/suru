# Suru

<img width="3316" height="1906" alt="image" src="https://github.com/user-attachments/assets/0792f2d3-f954-4505-8a16-415637a9f8ef" />

Suru is an agent orchestrator, drawing inspiration from tools like OpenCode and T3 Code.

It does not run LLMs by itself. Instead, it orchestrates other harnesses, such as Codex, Claude Code,
and Copilot, while providing a single consolidated view into their work. 

The goal is to provide a high-performance, configurable, and provider-agnostic agentic coding experience.

This repo is still very early in development - expect bugs and rough edges.

## Installation

Linux and macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/suru-ai/suru/main/scripts/install.sh | bash
```

Windows:

```powershell
irm https://raw.githubusercontent.com/suru-ai/suru/main/scripts/install.ps1 | iex
```

To uninstall, delete the binary: `~/.local/bin/suru`, or `%LOCALAPPDATA%\Programs\suru\suru.exe` on Windows.

## Features

The core "type a prompt and stuff happens" flow works as you might expect, so here are some of the additional
cool things Suru does:

- Cross-provider subagents - a Claude-managed Opus session can spin up a Codex-managed Astra session to review its work.
- Remote access - run `/serve` to generate an invite on the host, then paste the invite into `/pair` on another machine
  to allow the client to view all of the host's sessions. Reaching the host directly needs a network path between the
  two machines; where there is none, a Relay can carry the connection instead (below). Every connection, redeeming the
  invite included, is TLS 1.3 pinned to the host's key, with the joining machine presenting its own key in a client
  certificate.
- Relays - where your machines cannot reach each other directly, run a Relay of your own that each connects out to
  over HTTPS. It carries the Pairing's end-to-end TLS without being able to read it, and admits users by GitHub login.
  See [Running a Relay](docs/relay/README.md).
- `/sidekick` - an agent with a view over all your sessions across all of your machines. Use it to answer meta questions,
  or to orchestrate work across workspaces and remotes.

## Releasing

Releases are cut by running the Release workflow from GitHub Actions. It signs and notarizes the macOS binary with
secrets a maintainer sets up on a Mac by running `scripts/macos-signing-wizard.sh`, which walks through creating
each one and uploads them; run it again to renew the certificate or rotate the notarization key.
