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
The external product through which an agent operates, such as Codex, Copilot, or Claude. A Provider identifies that product rather than its underlying model vendor.
_Avoid_: Backend

**Provider Availability**:
Whether a Provider can be used at all right now, as Suru finds it: the external product installed, signed in to, and at a version Suru speaks. Availability is a fact about the environment rather than a choice, so every way a Provider can be unavailable names a condition the user fixes outside Suru, and Suru re-reads it only when the user asks for a Provider's Models again or turns to a surface that presents the Providers themselves. An unavailable Provider keeps its place wherever Providers are listed, with its reason on show, and none of its Models may be selected. Distinct from Provider Enablement, which is the user's own choice rather than something Suru discovers.
_Avoid_: Health, readiness, status

**Provider Enablement**:
Whether the user wants Suru to offer a Provider at all, carried as a Setting and enabled unless they say otherwise. A disabled Provider is one Suru leaves entirely alone: never asked for its Models, never checked for Availability, never asked to begin a Session — so it costs nothing and reports nothing. It is not offered where a user chooses an Agent, and a Session already bound to it cannot begin another Turn until it is enabled again, though work already under way is left to finish: Enablement governs what Suru does next rather than what it is doing. The user may disable every Provider, which leaves them nothing to select and is theirs to undo. Distinct from Provider Availability, which Suru discovers and the user fixes outside Suru; Enablement is the user's own choice and takes effect as soon as they make it.
_Avoid_: Provider toggle, active, installed

**Model**:
The language model selected for an Agent through its Provider.
_Avoid_: Engine

**Model Option**:
A Provider-advertised, Model-specific configuration dimension such as reasoning effort or speed. Model Options compose independently within an Agent Selection.
_Avoid_: Variant, trait, model setting

**Session**:
A workspace for conversation between a user and an agent. A Session is independently addressable, may be viewed from multiple clients, and may exist before an agent is selected. Its first Turn binds it to that Agent's Provider; later Agent Selections may change the Model and Model Options, but not the Provider.
_Avoid_: Chat, thread, conversation

**Title**:
The short line by which a Session is known wherever Sessions are listed, and the text a reader searches those listings by. A Title begins as the Session's first Prompt trimmed of the space around it — a real Title rather than a placeholder — and Suru replaces it once it has derived a better one through an Errand. Derivation is attempted once, when the first Prompt is admitted, and never again: a Session whose derivation was skipped, failed, or abandoned keeps the Title its Prompt gave it for good. A derived Title only replaces the Title it was derived from, so a Title since set by other means stands. A Setting decides whether Suru derives Titles at all, and which Agent Selection does the deriving.
_Avoid_: Name, subject, summary

**Emoji**:
A single emoji standing for a Session beside its Title, derived with that Title in the same Errand and carried as a typed property rather than written into the Title's own text, so searching a listing of Sessions matches the words a reader remembers rather than the character in front of them. A Session may have none, which every surface presenting Sessions draws as readily as it draws one.
_Avoid_: Icon, glyph — which a Marker and a Spinner already claim — avatar

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
The transition of a Turn or Activity into a terminal state — completed, failed, or interrupted — after which it accepts no further Provider output. A Session Settles in its own, reversible sense: set aside as done for now, by the user's say-so or on its own after long enough idle, and active again the moment it is prompted or the user unsettles it. Only the say-so is stored, as a marker stamped with the moment it was set. Settling on its own — **auto-settle**, governed by one Setting holding either how long being left alone has to be or the word that suspends it — is instead derived wherever Sessions are listed, from the Session's last activity: nothing is written down for it, no clock fires for it, and moving that Setting reclassifies every Session at once. Neither a Session nothing has moved since it was made nor one Suru cannot read auto-settles: the first has set nothing aside and the second has no activity Suru can see. Unsettling moves a Session's last activity to the moment the user reached for it, so auto-settle cannot put back what they just took off the shelf. Wherever Sessions are listed by liveness, the settled ones stand apart from the active ones.
_Avoid_: Finish, close, resolve; archive (for a settled Session)

**Prompt**:
User input submitted for delivery to an agent. A delivered Prompt becomes either the user Message that begins a Turn or a later user Message that steers its active Turn.
_Avoid_: Request, draft, message

**Errand**:
A single Provider call Suru makes for its own purposes rather than the user's: one Prompt in, one reply shaped by the schema the Errand asks for, carrying no Tools. An Errand belongs to no Session and appears in no Transcript, and it is never a Turn, because nothing about it is the user's work. A Provider runs an Errand however its own harness allows — without a Session where one-shot work is offered, and otherwise through a Session it starts and discards — and Suru stores nothing of it either way but the answer it asked for. An Errand that fails, times out, or answers outside its schema leaves no mark beyond the Log, because whatever asked for one always has something to fall back on.
_Avoid_: Background turn, side call, utility prompt

**Errand Selection**:
The Agent Selection a Provider declares for running Errands, chosen for cheapness and speed rather than capability, and resolved against that Provider's live Models each time an Errand runs, so a Model that has gone gives way to the Provider's default rather than failing the Errand. Whatever asks for an Errand decides which Provider runs it, and the Provider's own declaration then decides which Model: deriving a Title follows the Session's own Provider, and a Session that has selected no Provider runs no Errand at all. A Setting may pin one Agent Selection for every Session instead, which stands in front of the Provider's declaration rather than beside it — so the pinned Model is the one an Errand runs at, a Session that has selected no Provider is titled like any other, and the same resolution applies to the pin, a Model that has gone giving way to that Provider's default.
_Avoid_: Small model, cheap model, title model

**Message**:
User-visible content in a session attributed to the user or agent.
_Avoid_: Event, item

**Activity**:
User-visible progress, operational detail, or failure associated with a turn but not authored by the user or agent.
_Avoid_: Message, notification

**Tool**:
A provider-operated capability that performs work for an agent. User-visible Tool execution is represented as Activity.
_Avoid_: Function, action

**Skill**:
A Provider-offered body of task-specific guidance that an Agent may use and a user may explicitly select for a Prompt. Suru identifies an offered Skill to clients by an opaque identity and safe presentation metadata, leaving its native path or command inside the Provider. Distinct from a Tool, which performs work; a Skill guides how work is approached.
_Avoid_: Command, prompt template

**Skill Invocation**:
A user's explicit selection of a Skill for one Prompt, written as `$skill-name` in that Prompt and carried as a typed binding beside the original text. It remains bound to that Provider's Skill even when Providers differ in how they receive it; an unbound `$token` is ordinary text.
_Avoid_: Skill mention, skill command

**Skill Catalog**:
The current set of Skills a Provider offers a user in one Workspace, together with the Provider's effective Skill Invocation limits and Prompt delivery support. Before a Session's first Turn it follows the Agent Selection; afterwards it follows the Session's fixed Provider. It contains only user-invocable Skills and may change when the Provider's environment changes.
_Avoid_: Skill list, command catalog

**Reasoning**:
The account a Provider gives of an agent's thinking during a Turn, carried as Activity because it reports progress rather than the prose the agent authored for the user. A Reasoning block may carry a **title** — a short heading the Provider leads it with, kept beside the content as a typed property so a client can head a Fold with it instead of reading it out of the prose. Each heading the Provider leads a section with begins a new Reasoning block, so a block never carries more than one title. A block that settles with no title and no content stays stored, but a Transcript shows nothing for it. A Transcript hides the kind outright unless a reader asks for it, which is a Setting rather than view state: every block stays stored and keeps arriving either way, and while they are hidden a Transcript shows nothing for any of them — so hidden blocks, like empty ones, neither join nor end the run around them and leave no gap. Distinct from the Reasoning Effort Model Option, which tunes how much of it a Model does, and from the Setting deciding how much summary detail a Turn asks a Provider for, which is what arrives rather than what is shown. Suru's own name for the concept is Reasoning everywhere except the words a Transcript shows a reader, which deliberately say _Thinking_ while a block runs and _Thought_ once it settles.
_Avoid_: Chain of thought, and — outside a Transcript's own wording — thinking, thought

**Command**:
The Activity recording one command a Provider ran while working a Turn. Its command text is the command as a reader should see it: each Provider strips its own launcher plumbing — such as the shell wrapper it launches scripts through — before the Activity is recorded, so the stored text is the command itself, never the machinery around it. A command that arrives in a shape the Provider doesn't recognize as its own plumbing is recorded verbatim.
_Avoid_: Shell invocation, exec

**Transcript**:
The ordered, user-visible history of a Session: its Messages and Activities in presentation order.
_Avoid_: History, log, conversation

**Session Content Column**:
The main working column of an open Session: its Transcript, queued Prompts, latest-position affordance, composer extensions, composer, and composer footer. The Session header and the Landing sit outside it.
_Avoid_: Conversation column, transcript column

**Sidebar**:
The collapsible column a client shows beside its main view, listing Sessions with the active apart from the settled, searched by Title, and scoped to one Workspace or all of them. Whether it begins shown, and how wide its scope begins, are Settings; showing or hiding it afterwards is the reader's own view state, and a terminal too narrow for both it and the main view keeps the main view. The settled Sessions stand on a **shelf** below a **divider**: a rule the Sidebar draws only where something is settled, closing the active list and naming what the rows beneath it are. Shelf rows are slim where active rows are not, because settled work is history a reader keeps in view rather than work they are choosing between.
_Avoid_: Panel, drawer, session list, nav; archive or section (for the settled shelf)

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
One user-tunable value governing Suru's behavior, carrying a built-in default that applies whenever no Config Document pins it. Every Setting declares a scope: a **Client Setting** governs a client's presentation, and a machine-local Config Document may one day overlay it, while a **Server Setting** governs server or Provider behavior and follows only the server's own Config Documents. Every Setting also declares what it accepts: a **Fixed Setting** — almost all of them — accepts a set of values named up front, which is what lets a reader cycle one through them and lets Suru say exactly what to type where a value is rejected, while an **Open Setting** holds something Suru only discovers while running, such as an Agent Selection, and so names the values it can and describes the rest. Distinct from a Model Option, which is Provider-advertised rather than user-authored.
_Avoid_: Option, preference, config value

**Config Document**:
A file in which a user pins Settings. Config Documents stack in a fixed precedence order, and an edit Suru makes to one changes only the value it targets, leaving the rest of the document — its ordering, spacing, and comments — untouched.
_Avoid_: Settings file, preferences file, config
