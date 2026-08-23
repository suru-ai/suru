# Suru

Suru is an interactive workspace in which a user collaborates with an AI agent through a conversation.

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

**Landing**:
The view a client shows when no Session is open, carrying the Agent Selection a new Session will begin from and the composer its first Prompt is written in.
_Avoid_: Home, launch view, start screen, welcome screen

**Workspace**:
The working context in which an agent operates, initially rooted at a local directory.
_Avoid_: Project, working directory, location

**Turn**:
A unit of work that begins when a Prompt is delivered while a Session is idle and includes the resulting Agent activity. A Turn retains its effective Agent identity and may accept delivered steer Prompts without beginning another Turn. A Turn records when it began and when it Settled, so any surface reading it can state how long it worked.
_Avoid_: Request, exchange

**Settle**:
The transition of a Turn or Activity into a terminal state — completed, failed, or interrupted — after which it accepts no further Provider output.
_Avoid_: Finish, close, resolve

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

**Reasoning**:
The account a Provider gives of an agent's thinking during a Turn, carried as Activity because it reports progress rather than the prose the agent authored for the user. A Reasoning block may carry a **title** — a short heading the Provider leads it with, kept beside the content as a typed property so a client can head a Fold with it instead of reading it out of the prose. Each heading the Provider leads a section with begins a new Reasoning block, so a block never carries more than one title. A block that settles with no title and no content stays stored, but a Transcript shows nothing for it. A Transcript hides the kind outright unless a reader asks for it, which is a Setting rather than view state: every block stays stored and keeps arriving either way, and while they are hidden a Transcript shows nothing for any of them — so hidden blocks, like empty ones, neither join nor end the run around them and leave no gap. Distinct from the Reasoning Effort Model Option, which tunes how much of it a Model does, and from the Setting deciding how much summary detail a Turn asks a Provider for, which is what arrives rather than what is shown. Suru's own name for the concept is Reasoning everywhere except the words a Transcript shows a reader, which deliberately say _Thinking_ while a block runs and _Thought_ once it settles.
_Avoid_: Chain of thought, and — outside a Transcript's own wording — thinking, thought

**Command**:
The Activity recording one command a Provider ran while working a Turn. Its command text is the command as a reader should see it: each Provider strips its own launcher plumbing — such as the shell wrapper it launches scripts through — before the Activity is recorded, so the stored text is the command itself, never the machinery around it. A command that arrives in a shape the Provider doesn't recognize as its own plumbing is recorded verbatim.
_Avoid_: Shell invocation, exec

**Transcript**:
The ordered, user-visible history of a Session: its Messages and Activities in presentation order.
_Avoid_: History, log, conversation

**Truncation**:
The condition of a Message, command Activity, or Reasoning Activity whose stored content Suru's cap cut short of everything the Provider sent. Truncation is carried as a typed property beside the content rather than as text within it, so a client reads it as data and draws its own **truncation marker**: the line a Transcript shows in place of what the cap dropped.
_Avoid_: Elision, clipping

**Fold**:
The compact presentation a client's Transcript gives an entry whose full stored content remains available. A Fold is reversible, client-local view state: expanding it fully reveals everything stored, and folding never alters stored content. A folded entry shows a **fold marker**: the line indicating how much the Fold hides. A Fold may open in stages: a **Peek** is an intermediate step that reveals part of what the Fold hides — such as the tail of a command Activity's output — while the fold marker counts what remains hidden. Distinct from Truncation, which is a condition of the stored content itself; a single entry can carry both.
_Avoid_: Collapse, elision, hide; preview (for Peek)

**Group**:
The single-row presentation a client's Transcript gives a run of two or more adjacent Activities of the same groupable kind — commands or Reasoning. Like a Fold, a Group is reversible, client-local view state: grouping never alters stored content or presentation order, and any entry of another kind ends the run, though an entry the Transcript shows nothing for neither joins nor ends one. Membership follows the kind: a command joins only once it settles successfully — a failed or interrupted command ends the run, and a still-running command stays outside until it settles — while a Reasoning block belongs to its Group from the moment it starts, so a Reasoning Group forms live as thinking streams. Its marker leads with the latest member's title once the run has settled, and while the run is live with the latest title the run knows — held across a section the Provider has not headed yet, so the row always names the most recent topic rather than falling silent mid-run. Expansion follows the kind too: a command Group's members each keep their own Fold state, while a Reasoning Group opens straight onto every member's full content.
_Avoid_: Batch, merge, cell

**Turn Fold**:
The single-marker presentation a client's Transcript gives a settled Turn, hiding the work between the Turn's opening user Message and its outcome. Like a Fold, a Turn Fold is reversible, client-local view state, but it keys on the Turn's Settle rather than on entry adjacency, and it opens in one step rather than in stages. Its marker names how the Turn settled — worked, stopped, or failed — with the Turn's duration when known. A Turn's user Messages, its final agent Message, and a failed Turn's terminal Error Activity stay visible outside it; entries revealed by expanding keep their own Fold and Group state. The marker stands in the same place folded or expanded, because it is the row a reader clicks to move between the two: folded it stands for the work, and expanded it heads the work it opened onto. A Turn Fold also manages itself around the reader's attention, which a Fold does not: interrupting a Turn opens the fold it is about to settle into so the reader keeps their place, and a newer Turn beginning folds back the Turns before it that the reader had opened — the interrupted one included — because compressing past work is what the fold is for.
_Avoid_: Turn collapse, worked row

**Marker**:
The leading cell of an Activity's header row in a Transcript: a Spinner while the Activity is Active, the Activity's outcome glyph once it Settles. One cell with one contract, shared by every Activity kind that shows liveness. Distinct from a fold marker or truncation marker, which are whole rows.
_Avoid_: Status icon, prefix glyph

**Spinner**:
The animated glyph a client shows where work is live right now — in the Marker of an Active Activity, and beside any other live-work state a client surfaces. A Spinner only ever animates presentation: it carries no state of its own beyond which frame is showing.
_Avoid_: Throbber, loading indicator

**Resume State**:
Provider-owned data, opaque to Suru, that lets a Session continue with its Provider after a restart. Without it a restored Session is viewable but cannot continue where it left off.
_Avoid_: Thread mapping, provider cache

**Channel**:
The build variant (release or a development channel) whose sessions and runtime state are kept separate so development runs never touch real data.
_Avoid_: Environment, profile

**Log**:
The operator-facing diagnostic record a Suru process writes about its own run. A Log is about Suru's behavior, never about conversation content — the user-visible history of a Session is its Transcript.
_Avoid_: Diagnostics, telemetry, trace

**Notice**:
A transient, one-line message a client shows about Suru's own behavior rather than about a Session. A Notice is dismissed by the reader's next interaction and never returns for the rest of the run, and it points at the Log rather than carrying the detail itself.
_Avoid_: Banner, toast, alert, diagnostic

**Setting**:
One user-tunable value governing Suru's behavior, carrying a built-in default that applies whenever no Config Document pins it. Every Setting declares a scope: a **Client Setting** governs a client's presentation, and a machine-local Config Document may one day overlay it, while a **Server Setting** governs server or Provider behavior and follows only the server's own Config Documents. Distinct from a Model Option, which is Provider-advertised rather than user-authored.
_Avoid_: Option, preference, config value

**Config Document**:
A file in which a user pins Settings. Config Documents stack in a fixed precedence order, and an edit Suru makes to one changes only the value it targets, leaving the rest of the document — its ordering, spacing, and comments — untouched.
_Avoid_: Settings file, preferences file, config
