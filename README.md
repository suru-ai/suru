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

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/suru-ai/suru/main/scripts/install.ps1 | iex
```

Either installs the latest release for your user, without needing elevation, and offers to add it to your `PATH`.
Run it again to upgrade. Set `SURU_VERSION` (e.g. `v0.1.1`) to install a particular release, `SURU_INSTALL_DIR` to
choose where the binary goes, and `SURU_YES=1` to skip the questions.

Builds are published for x86_64 and aarch64 Linux (glibc), Apple silicon macOS, and x64 and ARM64 Windows.

To uninstall, delete the binary: `~/.local/bin/suru`, or `%LOCALAPPDATA%\Programs\suru\suru.exe` on Windows.

## Features

The core "type a prompt and stuff happens" flow works as you might expect, so here are some of the additional
cool things Suru does:

- Cross-provider subagents - a Claude-managed Opus session can spin up a Codex-managed Astra session to review its work.
- Remote access - run `/serve` to generate an invite on the host, then paste the invite into `/pair` on another machine
  to allow the client to view all of the host's sessions. This requires a network path to already exist between the two machines.
  This uses mTLS after the initial connection.
