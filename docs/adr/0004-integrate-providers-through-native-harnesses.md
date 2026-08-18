# Integrate providers through native harnesses

Chidori integrates each Provider through its native harness, beginning with Codex app-server and later the Copilot SDK, rather than calling OpenAI-compatible model endpoints directly. Provider-native Sessions, tools, streaming, and control are normalized at a typed Provider Session boundary so Chidori owns its domain model without reimplementing each harness.
