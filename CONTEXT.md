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
A workspace for conversation between a user and an agent. A Session is independently addressable, may be viewed from multiple clients, and may exist before an agent is selected. Its first Turn binds it to that Agent's Provider; later Agent Selections may change the Model and Model Options, but not the Provider. A Subagent's Session is a child of the Session whose Turn spawned it: viewed, streamed, and stored like any other, to any depth of children of its own, but reachable only through its parent — never listed where Sessions are listed, never offered a Prompt, and deleted along with the parent whose deletion it shares.
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
The working context in which an agent operates, initially rooted at a local directory. There is one reading of that directory: the server canonicalizes the Workspace it roots a Session at and the one it narrows a listing by, and a client reads the directory it launches in — and any it is later pointed at — the same way, so the two never hold different spellings of the same place. A directory that cannot be read that way stands as given rather than refusing to start.
_Avoid_: Project, working directory, location

**Turn**:
A unit of work that ordinarily begins when a Prompt is delivered while a Session is idle and includes the resulting Agent activity — a Continuation being the one Turn that begins without one. A Turn retains its effective Agent identity and may accept delivered steer Prompts without beginning another Turn. A Turn records when it began and when it Settled, so any surface reading it can state how long it worked. A Turn's Settle is the Provider's own boundary, so a Turn may Settle while Subagents it spawned work on; the Session is not idle again until they too have settled.
_Avoid_: Request, exchange

**Continuation**:
The one kind of Turn that begins without a Prompt: Suru begins one itself when Provider output arrives while no Turn is active, owed to an earlier Turn's Subagents still working after that Turn Settled. A Continuation settles like any Turn, and one still open when the next Prompt is delivered settles then, so it never stands in that Prompt's way.
_Avoid_: Synthetic turn, background turn, ghost turn

**Settle**:
The transition of a Turn or Activity into a terminal state — completed, failed, or interrupted — after which it accepts no further Provider output. A Session Settles in its own, reversible sense: set aside as done for now, by the user's say-so or on its own after long enough idle, and active again the moment it is prompted or the user unsettles it. Only the say-so is stored, as a marker stamped with the moment it was set. Settling on its own — **auto-settle**, governed by one Setting holding either how long being left alone has to be or the word that suspends it — is instead derived wherever Sessions are listed, from the Session's last activity: nothing is written down for it, no clock fires for it, and moving that Setting reclassifies every Session at once. Neither a Session nothing has moved since it was made nor one Suru cannot read auto-settles: the first has set nothing aside and the second has no activity Suru can see. Unsettling moves a Session's last activity to the moment the user reached for it, so auto-settle cannot put back what they just took off the shelf. Wherever Sessions are listed by liveness, the settled ones stand apart from the active ones.
_Avoid_: Finish, close, resolve; archive (for a settled Session)

**Usage**:
The record of the tokens a Turn consumed, kept in five parts — fresh input, cache reads, cache writes, output, and the reasoning within that output — with any part a Provider does not report simply absent, never guessed at zero. A Turn records its Usage the way it records when it began and Settled, and a failed or interrupted Turn keeps whatever Usage it accrued, because Usage answers what a Session has consumed rather than what it got for it. A Session's Usage is the sum over its Turns' Usage, and the total a surface shows for a Session includes its Subagent subtree, while each child Session keeps its own.
_Avoid_: Token count, consumption, spend

**Cost**:
The dollar figure attached to a Turn's Usage, fixed when that Usage is recorded and never restated against later prices, so a historical Cost stays a fact about the past. A Cost the Provider states itself outranks one Suru estimates from a rate table, and where neither exists the Cost is absent — shown as nothing rather than as zero, so free and unknown never blur. Cost is the API-equivalent figure even where a subscription means nothing marginal was billed; its Cost Basis says where the number came from.
_Avoid_: Price (that is a rate), spend, billing

**Cost Basis**:
Where a Cost came from: **Reported** when the Provider itself stated the figure, **Estimated** when Suru computed it from a rate table. Basis records who computed the number rather than whether money changed hands — a Reported Cost under a subscription may still have billed nothing.
_Avoid_: Cost source, cost type

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

**Subagent**:
An agent to which a Turn's Agent delegates work through its Provider, running its own conversation in its own Session — a child of the Session whose Turn spawned it. The parent's Transcript records each Subagent as an Activity of its own kind: one row naming the Subagent and what it was asked to do, wearing the usual Marker while it works and its outcome and duration once it settles, and standing — live or settled — as the way into the Subagent's Session. That row is all the parent's Transcript carries of it: the Subagent's work belongs to its own Transcript, never interleaved into the parent's. A Subagent may outlive the Turn that spawned it; while any Subagent still works the Session is still Working, and output one provokes after its Turn Settled lands in whatever Turn is active or begins a Continuation. Subagents may spawn Subagents, each recorded the same way one level down. Interrupting a Session whose Subagents still work stops them along with whatever else the Session is doing, and a single Subagent may be stopped on its own where its Provider allows it; neither asks before acting, as interrupting never does.
_Avoid_: Task, child agent, background agent, worker

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

**File Change**:
The Activity recording one Provider-reported operation over one or more files in the Workspace.
_Avoid_: Patch, file edit

**Transcript**:
The ordered, user-visible history of a Session: its Messages and Activities in presentation order.
_Avoid_: History, log, conversation

**Session Content Column**:
The main working column of an open Session: its Transcript, queued Prompts, latest-position affordance, composer extensions, composer, and composer footer. The Session header and the Landing sit outside it.
_Avoid_: Conversation column, transcript column

**Sidebar**:
The collapsible column a client shows beside its main view, listing Sessions with the active apart from the settled, searched by Title, and scoped to one Workspace or all of them. Whether it begins shown, and how wide its scope begins, are Settings; showing or hiding it afterwards is the reader's own view state, and a terminal too narrow for both it and the main view keeps the main view. An active row's **right slot** carries what that Session is doing: while its latest Turn has not Settled — or Subagents it spawned still work on past it — it reads **Working** with the duration the work has been running, and otherwise the compact time since the Session last moved. Working is the first of a fuller precedence of status labels, the rest of which arrive with the state behind them. The settled Sessions stand on a **shelf** below a **divider**: a rule the Sidebar draws only where something is settled, closing the active list and naming what the rows beneath it are. Shelf rows are slim where active rows are not, because settled work is history a reader keeps in view rather than work they are choosing between. The shelf is bounded: it opens on its first rows and the rest of the tail stands behind an affordance the reader asks for, batch by batch, until the shelf is whole. A **search box** at the top of the Sidebar narrows it by Title, and while it carries a query both shelves stand down: the Sidebar answers with one flat list of results, in the order the shelves would have drawn them and each keeping the shape its shelf gives it. Under it a **selector** says which Workspace the Sidebar is answering for and opens the ways into the reader's work: all of it first, then every Workspace it has Sessions in and the one the client itself runs in, whether or not there is work there yet. Choosing one narrows both shelves and the results a query answers with; the Sidebar goes on listing the whole body of work either way, because the entries are read off that listing and narrowing what it asks for would take the other Workspaces off the selector along with their Sessions. Beside the selector an **add-Workspace affordance** opens a **path entry**, which stands in place of the list and takes what the reader types: a path they give relative is read from the Workspace they are working in, and the directory it names — read the way the server reads one, so a client can never narrow past the work it has just rooted — becomes the client's current Workspace — the root of the Sessions they make next, what current-Workspace scope comes to mean, and what the selector narrows to. A path standing at a file or at nothing is refused where the reader can see it and nothing moves. The rows answer a pointer as readily as the keys: pressing one opens the Session it stands for, and a row asked for its **context menu** is offered the shelf it is not on — set aside, or brought back — along with deleting the Session, which asks again before it acts. A Sidebar on screen is as true as the server without the reader asking: every change the session-catalog stream reports — a Session made, retitled, deleted, set aside, brought back, its latest Turn begun or settled, or a whole catalog reconciled after a reconnection — is taken as it happens, and the Sidebar asks for its listing again to carry what the change itself does not say. Catching up is not the reader looking again, so it leaves them on the row they were on, the shelf as deep as they walked it, and whatever they had opened standing.
_Avoid_: Panel, drawer, session list, nav; archive or section (for the settled shelf)

**Subagent Picker**:
The docked list a client opens over the composer to browse the open Session's working Subagents, drawn as the tree they spawned in. It opens only while there is something to browse, so the key that opens it stays inert otherwise; moving through it and choosing an entry opens that Subagent's Session, a working entry may be stopped from its row, and closing it lands back where it opened. Settled Subagents are not its concern — they are reached from their rows in the Transcript.
_Avoid_: Agent panel, roster, subagent list

**Workspace Picker**:
The centered list a client opens over its view to switch Workspaces, listing every Workspace the user's Sessions have rooted in plus the one the client is working in — the current one first, the rest ordered by which held work most recently — and searched by name. Choosing one makes it the client's current Workspace — the root of the Sessions they make next, the Workspace the Skill Catalog answers for, what current-Workspace scope comes to mean, and the base a relative path resolves against — and opens the Landing there, leaving whatever Session was open to its own work and the Sidebar's chosen scope where the reader put it; in that last respect it deliberately differs from the Sidebar's path entry, which narrows the Sidebar it lives in as it switches. A picked Workspace whose directory no longer stands is refused where the reader can see it, exactly as the path entry refuses one, and nothing moves. Every row is a concrete place to root a Session: the Picker offers neither an all-Workspaces row nor a way to add a Workspace it doesn't know.
_Avoid_: Project picker, project list, workspace switcher

**Truncation**:
The condition of a Message, command Activity, or Reasoning Activity whose stored content Suru's cap cut short of everything the Provider sent. Truncation is carried as a typed property beside the content rather than as text within it, so a client reads it as data and draws its own **truncation marker**: the line a Transcript shows in place of what the cap dropped.
_Avoid_: Elision, clipping

**Fold**:
The compact presentation a client's Transcript gives an entry whose full stored content remains available. A Fold is reversible, client-local view state: expanding it fully reveals everything stored, and folding never alters stored content. A folded entry shows a **fold marker**: the line indicating how much the Fold hides. A Fold may open in stages: a **Peek** is an intermediate step that reveals part of what the Fold hides — such as the tail of a command Activity's output — while the fold marker counts what remains hidden. Distinct from Truncation, which is a condition of the stored content itself; a single entry can carry both.
_Avoid_: Collapse, elision, hide; preview (for Peek)

**Group**:
The reversible, client-local presentation a Transcript gives a run of two or more adjacent Activities of the same groupable kind — commands or Reasoning — without changing stored content or presentation order. Successful commands contribute to a command Group's marker, while an adjacent Active command grows visibly beneath it until success merges it upward or failure leaves it outside; Reasoning belongs from the moment it starts, and opening either kind reveals its members in that kind's own Fold presentation.
_Avoid_: Batch, merge, cell

**Turn Fold**:
The single-marker presentation a client's Transcript gives a settled Turn, hiding the work between the Turn's opening user Message and its outcome. Like a Fold, a Turn Fold is reversible, client-local view state, but it keys on the Turn's Settle rather than on entry adjacency, and it opens in one step rather than in stages. Its marker names how the Turn settled — worked, stopped, or failed — with the Turn's duration when known. A Turn's user Messages, its final agent Message, and a failed Turn's terminal Error Activity stay visible outside it; entries revealed by expanding keep their own Fold and Group state. The marker stands in the same place folded or expanded, because it is the row a reader clicks to move between the two: folded it stands for the work, and expanded it heads the work it opened onto. A Turn Fold also manages itself around the reader's attention, which a Fold does not: interrupting a Turn opens the fold it is about to settle into so the reader keeps their place, and a newer Turn beginning folds back the Turns before it that the reader had opened — the interrupted one included — because compressing past work is what the fold is for.
_Avoid_: Turn collapse, worked row

**Marker**:
The leading cell of an Activity row in a Transcript: a Spinner while the Activity is Active, the Activity's outcome glyph once it Settles. Every row belonging to a multi-row File Change repeats the Activity's Marker so those peer rows keep one visual shape. Distinct from a fold marker or truncation marker, which are whole rows.
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
