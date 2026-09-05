# Provider Questionnaires

The behavior below was agreed during the design interview, and shared understanding was confirmed. Implementation is deferred at the user's request.

## Agreed behavior

- Support native structured user-input requests from Codex, Copilot, and Claude through generic Provider interfaces. Permission approvals and questions in ordinary Agent prose are outside this feature.
- Use Questionnaire for the whole request, Question for an individual item, and Answer for the user's submitted response to the whole request.
- Preserve native capabilities through the shared interface: batches, single or multiple selection, free text, and secret input where supported. Do not offer answer forms a Provider cannot receive.
- Present an answering panel in the composer area and record the Questionnaire in the Transcript. Preserve the composer's existing draft, and allow Transcript browsing and Session switching.
- Show one Question at a time with back/next navigation and a final review before submitting the whole Answer.
- Allow hiding the panel to return to the normal composer, with a visible pending-question indicator. Hiding sends nothing. Whether the Agent waits or continues follows the Provider's native behavior.
- Any Client viewing the Session may answer. The first accepted submission wins; other Clients see the submitted Answer and cannot overwrite it with stale submissions.
- Pending requests survive closing the TUI while the server and Provider request remain alive. After server restart, retain history but offer answering only if the Provider restores the request.
- Mask secret input while editing and exclude secret values from Suru's Transcript and logs. Record only that it was answered, and send the value to the requesting Provider. Ordinary Answers remain readable in history.
- Keep concurrent Questionnaires individually addressable, initially showing the oldest pending request. Users may switch between them; arrivals never replace an Answer being composed.
- Arrivals show a pending indicator without stealing keyboard focus. Entering the answering panel is explicit. Session listings mark pending questions; a Subagent's pending question also marks its parent, and answering happens in the child Session.
- Keep unsubmitted Answer drafts locally in the Client across panel hiding and Session switching. Discard them on Client exit or when the request becomes unavailable. Do not synchronize drafts between Clients; secret drafts stay in memory only.
- Present a completed Questionnaire as one compact, expandable Activity showing outcome and question count. Expansion reveals Questions and their submitted answers, with secrets replaced by “Answered.” Pending Questionnaires remain visible and offer an action to open the answering panel.
- Highlight recommended choices without selecting them. Require explicit answers to required Questions; allow omission only where the native contract supports it. Validate before submission and show errors beside the relevant Question.
- Introduce no automatic countdown. Requests remain pending until answered, explicitly declined, interrupted, or withdrawn by the Provider; do not reproduce Codex's client-side auto-resolution countdown.
- Offer a separate **Decline questionnaire** action mapped to the Provider's native no-answer or refusal mechanism. Declining need not end the Turn. Escape hides the panel; the existing interrupt command stops work.
- When the Provider withdraws a request or its owning Turn ends, immediately disable submission, discard the draft, and retain an outcome explaining why answering is unavailable. A pending Questionnaire never holds a finished Turn open.
- On submission failure, preserve the draft while the request remains live. Reconcile server acceptance before enabling retry and prevent duplicate delivery. If delivery to the Provider is uncertain, show that uncertainty rather than claiming success or automatically resending.

See [ADR-0019](adr/0019-separate-questionnaire-history-from-live-provider-requests.md) for the distinction between durable history and a live request.

## Provider integration evidence

- Codex currently rejects `item/tool/requestUserInput` in Suru. Its native request supports batches, option metadata, free text, secret input, and a blocking hint. Core awaits a response even when the request is marked nonblocking; that hint permits native UI auto-resolution rather than guaranteeing continued Agent work. Native no-answer responses and Turn interruption are distinct. App-server cancels pending requests at Turn boundaries. Reference: `references/codex/codex-rs/core/src/session/mod.rs`, `references/codex/codex-rs/app-server/src/bespoke_event_handling.rs`, and `references/codex/codex-rs/tui/src/bottom_pane/request_user_input/mod.rs`.
- Copilot's Rust SDK exposes a user-input handler on both create and resume. Requests contain one question, optional choices, and a freeform flag. Responses distinguish a selected choice from freeform text; returning no response is supported. Suru currently does not register that handler.
- Claude's native `AskUserQuestion` supports batches and multiple selection. Its permission callback receives these requests even under bypass permissions, so supporting questions does not require changing Suru's permission posture. Suru must configure the stdio permission callback and handle correlated control responses. Declining and interrupting are distinct. Sources: [permission evaluation](https://code.claude.com/docs/en/agent-sdk/permissions), [user input](https://code.claude.com/docs/en/agent-sdk/user-input), and [SDK control handling](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py).

## Implementation boundaries

Normalize requests and responses at the typed Provider Session boundary. The server owns live request identity and submission arbitration; Clients own local drafts and presentation. Carry Questionnaire Activity through typed Transcript projection, and expose visual actions through semantic command IDs, following the repository's existing UI extension conventions.

Validate Provider mappings, live request settlement, concurrent Client submissions, failure reconciliation, secret-value exclusion, and the answering flow across all three Providers. Tests must use platform-correct fixtures and injected short timings, and run through `cargo nextest run`.
