---
status: partially superseded by ADR-0006 (durable history)
---

# Keep ephemeral sessions on the shared server

Chidori keeps each Session authoritative on the reusable local server and independently addressable by Session ID rather than storing conversation state in a TUI or sharing one global Session. Multiple clients may observe the same Session while keeping drafts, cursors, focus, and scrolling local; server replacement ends all Sessions. This preserves a thin-client command/event boundary for future providers without prematurely committing to durable history.
