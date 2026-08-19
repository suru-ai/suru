# Chidori

Chidori is an interactive workspace in which a user collaborates with an AI agent through a conversation.

## Language

**Agent**:
The configured participant the user addresses, backed by a Provider and Model.
_Avoid_: Assistant, bot

**Agent Selection**:
The Provider, Model, and Model Option configuration a Session will use when its next Turn begins. It follows the Provider's effective selection once resolved, while an active Turn retains the Agent it began with.
_Avoid_: Model setting, model choice

**Provider**:
The external product through which an agent operates, such as Codex or Copilot. A Provider identifies that product rather than its underlying model vendor.
_Avoid_: Backend

**Model**:
The language model selected for an Agent through its Provider.
_Avoid_: Engine

**Model Option**:
A Provider-advertised, Model-specific configuration dimension such as reasoning effort or speed. Model Options compose independently within an Agent Selection.
_Avoid_: Variant, trait, model setting

**Session**:
A workspace for conversation between a user and an agent. A Session is independently addressable, may be viewed from multiple clients, and may exist before an agent is selected.
_Avoid_: Chat, thread, conversation

**Workspace**:
The working context in which an agent operates, initially rooted at a local directory.
_Avoid_: Project, working directory, location

**Turn**:
A unit of work that begins when a Prompt is delivered while a Session is idle and includes the resulting Agent activity. A Turn retains its effective Agent identity and may accept delivered steer Prompts without beginning another Turn.
_Avoid_: Request, exchange

**Prompt**:
User input submitted for delivery to an agent. A delivered Prompt becomes either the user Message that begins a Turn or a later user Message that steers its active Turn.
_Avoid_: Request, draft, message

**Message**:
User-visible content in a session attributed to the user or agent.
_Avoid_: Event, item

**Activity**:
User-visible progress, operational detail, or failure associated with a turn but not authored by the user or agent.
_Avoid_: Message, notification

**Tool**:
A provider-operated capability that performs work for an agent. User-visible Tool execution is represented as Activity.
_Avoid_: Function, action

**Transcript**:
The ordered, user-visible history of a Session: its Messages and Activities in presentation order.
_Avoid_: History, log, conversation

**Resume State**:
Provider-owned data, opaque to Chidori, that lets a Session continue with its Provider after a restart. Without it a restored Session is viewable but cannot continue where it left off.
_Avoid_: Thread mapping, provider cache

**Channel**:
The build variant (release or a development channel) whose sessions and runtime state are kept separate so development runs never touch real data.
_Avoid_: Environment, profile
