# Integrate providers through native harnesses

Suru integrates each Provider through its native harness, beginning with Codex app-server and later the Copilot SDK, rather than calling OpenAI-compatible model endpoints directly. Provider-native Sessions, tools, streaming, and control are normalized at a typed Provider Session boundary so Suru owns its domain model without reimplementing each harness.
