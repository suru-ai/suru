You have access to the following codebases under ./references/ you should use for inspiration
- t3-code - The main inspiration for the core provider setup & server architecture
- opencode - Use as a reference for how the TUI should look and function
- codex - OpenAI's CLI application for using thier models, use when building our Codex integration
- copilot-sdk - GitHub Copilot SDK. Use for building the Copilot integrations

To start we will only support the Codex and Copilot providers, but additional providers may be added at a later date.
Try to build all functionality supporting both providers to avoid building interfaces that are not generic enough to
support others down the line.
