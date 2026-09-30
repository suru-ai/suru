# Suru

<img width="3316" height="1906" alt="image" src="https://github.com/user-attachments/assets/0792f2d3-f954-4505-8a16-415637a9f8ef" />

Suru is an agent orchestrator, drawing inspiration from tools like OpenCode and T3 Code.

It does not run LLMs by itself. Instead, it orchestrates other harnesses, such as Codex, Claude Code,
and Copilot, while providing a single consolidated view into their work. 

The goal is to provide a high-performance, configurable, and provider-agnostic agentic coding experience.

This repo is still very early in development - expect bugs and rough edges.

## Installation

TBC

## Features

The core "type a prompt and stuff happens" flow works as you might expect, so here are some of the additional
cool things Suru does:

- Cross-provider subagents - a Claude-managed Opus session can spin up a Codex-managed Astra session to review its work.
- Remote access - run `/serve` to generate an invite on the host, then paste the invite into `/pair` on another machine
  to allow the client to view all of the host's sessions. This requires a network path to already exist between the two machines.
  This uses mTLS after the initial connection.
