# Bound a Sidekick by what it is offered rather than by Approvals

A Sidekick's Tools never ask an Approval, as no Broker Tool does, even though they prompt and interrupt other Sessions and change Settings. What keeps that safe is the set itself: a Sidekick is offered no Tool that deletes a Session, decides an Approval, changes an Approval Posture, or touches the Settings governing Serving or a Pairing, and no Tool acts on a Session of a Sidekick Workspace. The acts left out are the ones through which one Agent could widen what another is allowed, destroy work, or open the machine to another; with them gone, everything a Sidekick can do is something the user can see attributed in a Transcript and undo.

A Sidekick may answer a Questionnaire, because an Answer is input rather than consent, and its Answer is attributed as its Prompts are.

## Considered Options

- **Ask an Approval for each mutating Tool.** Rejected: Approvals are Provider-native (ADR 0026), so asking per Tool means three mechanisms — Claude's allow-list, Codex's per-server approval mode, Copilot's server-name check — each kept in step with the tool list, to gate acts the user summoned the Sidekick to perform.
- **Offer everything and rely on the Sidekick's own Approval Posture.** Rejected: a Sidekick deciding another Session's Approvals would make every posture on the Server only as strict as the Sidekick's.

## Consequences

- Adding delete, Approvals, or posture to a Sidekick later reopens this decision rather than extending a list.
- The exclusions are enforced where the Tools are served, and for a Remote's Sidekick Workspace by the Remote, not by instructions to the Agent.
