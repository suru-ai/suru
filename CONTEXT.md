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
Whether a Provider can be used at all right now, as Suru finds it: the external product installed, signed in to, and at a version Suru speaks. Availability is a fact about the environment rather than a choice, so every way a Provider can be unavailable names a condition the user fixes outside Suru, and Suru re-reads it only when the user asks for a Provider's Models again, turns to a surface that presents the Providers themselves, or connects to a server that has not yet asked that Provider since it started. An unavailable Provider keeps its place wherever Providers are listed, with its reason on show, and none of its Models may be selected. Distinct from Provider Enablement, which is the user's own choice rather than something Suru discovers.
_Avoid_: Health, readiness, status

**Provider Enablement**:
Whether the user wants Suru to offer a Provider at all, carried as a Setting and enabled unless they say otherwise. A disabled Provider is one Suru leaves entirely alone: never asked for its Models, never checked for Availability, never asked to begin a Session — so it costs nothing and reports nothing. It is not offered where a user chooses an Agent, and a Session already bound to it cannot begin another Turn until it is enabled again, though work already under way is left to finish: Enablement governs what Suru does next rather than what it is doing. The user may disable every Provider, which leaves them nothing to select and is theirs to undo. Distinct from Provider Availability, which Suru discovers and the user fixes outside Suru; Enablement is the user's own choice and takes effect as soon as they make it.
_Avoid_: Provider toggle, active, installed

**Model**:
The language model selected for an Agent through its Provider. A Subagent's actual Model is known only from Provider evidence; its parent's Model does not establish its own.
_Avoid_: Engine

**Model Catalog**:
The Models each Provider currently offers, together with their names and Model Options, as Suru last learned them from the Provider. Suru remembers the last catalog it learned across restarts so it can present Models by name before asking the Provider again, and asks each Provider anew when a user connects to a server that has not yet asked it since it started. A remembered catalog is the same catalog, not a separate copy, and a Model's name always reflects the latest catalog. Distinct from Provider Availability, which is never remembered: Suru re-reads it rather than replaying a past condition.
_Avoid_: Models cache, model list, snapshot

**Model Option**:
A Provider-advertised, Model-specific configuration dimension such as reasoning effort or speed. Model Options compose independently within an Agent Selection.
_Avoid_: Variant, trait, model setting

**Session**:
A workspace for conversation between a user and an agent. A Session is independently addressable, may be viewed from multiple clients, and may exist before an agent is selected. Its first Turn binds it to that Agent's Provider; later Agent Selections may change the Model and Model Options, but not the Provider. A Subagent's Session is a child of the Session whose Turn spawned it: viewed, streamed, and stored like any other, to any depth of children of its own, but reachable only through its parent — never listed where Sessions are listed, never offered a Prompt, and deleted along with the parent whose deletion it shares.
_Avoid_: Chat, thread, conversation

**Title**:
The short line by which a Session is known both while viewing it and wherever Sessions are listed, and the text a reader searches those listings by. A Title begins as the Session's first Prompt trimmed of the space around it — a real Title rather than a placeholder — and Suru replaces it once it has derived a better one through an Errand. Derivation is attempted once, when the first Prompt is admitted, and never again: a Session whose derivation was skipped, failed, or abandoned keeps the Title its Prompt gave it for good. A derived Title only replaces the Title it was derived from, so a Title since set by other means stands. A Setting decides whether Suru derives Titles at all, and which Agent Selection does the deriving.
_Avoid_: Name, subject, summary

**Emoji**:
A single emoji standing for a Session beside its Title, derived with that Title in the same Errand and carried as a typed property rather than written into the Title's own text, so searching a listing of Sessions matches the words a reader remembers rather than the character in front of them. A Session may have none, which every surface presenting Sessions draws as readily as it draws one. Whether the ones a Session does carry are drawn at all is a Setting, off until the reader turns it on: hiding them is presentation and nothing else, so an Emoji goes on being derived, stored, and carried to every client, and turning them back on reveals what Suru already holds.
_Avoid_: Icon, glyph — which a Marker and a Spinner already claim — avatar

**Landing**:
The view a client shows when no Session is open, carrying the Agent Selection a new Session will begin from, its intended execution location, and the composer its first Prompt is written in. Beneath the composer it names the Workspace by its presented root, followed by the selected Worktree's Checkout State as a Sidebar row draws it and, when working in a subdirectory, the path relative to that Worktree's root; the user may choose an existing Worktree or ask Suru to create one on submit, with that pending intent shown here and its branch, starting commit, and location managed by Suru.
_Avoid_: Home, launch view, start screen, welcome screen

**Provisional Session**:
The Session view a client shows from the moment the Landing's first Prompt is submitted until the Server answers with the Session it made, drawn from what the client already knows — the Prompt as a user Message, the Title the Prompt gives, the Execution Directory, the Agent Selection, and the Working Indicator — before any of it is confirmed. It is that client's own claim, never listed in the Sidebar, and replaced in place by the real Session when it arrives; its Working Indicator carries no elapsed time, because only the Server knows when Working began. When a new Worktree is requested, the view appears immediately and its indicator says **Creating worktree** without elapsed time through checkout creation and destination Skill discovery, then uses the ordinary Session-start indicator. Worktree preparation failure keeps this view and its Prompt in place for retry, reusing any retained Worktree. Interrupting Worktree preparation prevents Prompt admission and returns its text to the composer; any ongoing checkout operation may finish and its Worktree is retained. Its composer takes a draft but delivers nothing until the Session arrives, and a newer route abandons it without pulling the client back. If the Server refuses, the view stands with the user Message in place and a client-local, transcript-shaped **Error: Could not create Session:** row where the Working Indicator was, saying that Enter retries and that a new prompt may be typed instead: an empty submit retries the same Prompt, and text submitted replaces it as a new Prompt. Leaving a failed Provisional Session discards it and keeps its text as the Landing's draft.
_Avoid_: Optimistic session, pending session, draft session, creating state

**Workspace**:
The working context that groups Sessions on one Server by their shared Repository, including its Worktrees and their subdirectories, or by an individual directory outside source control. Separate clones and nested Repositories are separate Workspaces; a Workspace is presented by its main root, bare root, or repository metadata location when the main root is unknown, without making that label its identity or its Sessions' Execution Directory.
_Avoid_: Project, working directory, location

**Repository**:
The local source-controlled body of work a Workspace belongs to, including its related working copies. Sharing a remote address does not make separate clones the same Repository.
_Avoid_: Remote, origin, project

**Worktree**:
A Git Repository's working copy, either its main working copy or a linked one elsewhere on the same Server, shared by any Sessions that work within it. Every Worktree belongs to its Repository's Workspace, including those created outside Suru; settling a Session leaves it in place, while deleting the last Session that references a Managed Worktree can make it Reclaimable.
_Avoid_: Workspace (for an individual linked working copy)

**Managed Worktree**:
A linked Worktree that Suru created for a Session, living under the Repository's managed container on a branch Suru named and started from the commit the Session was created against. Only Managed Worktrees are ever Reclaimed; a linked Worktree the user created elsewhere is theirs to remove.
_Avoid_: temporary worktree, auto worktree, sandbox

**Reclaimable**:
A Managed Worktree eligible for Reclaim because no Session or unfinished preparation references it, every Session that references it has been inactive past the configured threshold, or its failed preparation is older than that threshold. Working Sessions, changes Git would need force to discard, initialized submodules, and Git locks Suru did not place make it ineligible.

**Reclaim**:
The Server's unattended removal of a Reclaimable Managed Worktree, distinct from explicit removal that the user asks for and confirms. It retains the branch unless fully merged into its recorded base, and leaves affected Sessions' histories and settlement unchanged so their next Prompt can recover the Worktree.
_Avoid_: cleanup, prune, sweep, garbage collection, expire, retire

**Execution Directory**:
The directory in which a Session's Agent works, which may be a Worktree root or a subdirectory and is fixed from its first Turn onward. Grouping a Session under its Workspace preserves this directory, including for Sessions that already exist; working elsewhere begins another Session.
_Avoid_: Workspace root, project directory

**Checkout State**:
The current branch or detached commit of a Worktree, shared by the Sessions that work within it rather than remembered separately for each Session. A Sidebar row presents this current state with the main Worktree implicit, appends **(worktree)** after a linked Worktree's branch name, or explicitly marks the Worktree unavailable when its state cannot be read.
_Avoid_: Session branch, original branch

**Turn**:
A unit of work that ordinarily begins when a Prompt is delivered while a Session is idle and includes the resulting Agent activity — a Continuation being the one Turn that begins without one. A Turn retains its effective Agent identity and may accept delivered steer Prompts without beginning another Turn. A Turn records when it began and when it Settled, so any surface reading it can state how long it worked. A Turn's Settle is the Provider's own boundary, so a Turn may Settle while Subagents it spawned work on; the Session is not idle again until they too have settled.
_Avoid_: Request, exchange

**Continuation**:
The one kind of Turn that begins without a Prompt: the Provider resumes work after a prior Turn Settled, including work provoked by a background Command or Subagent, or Suru receives output owed to an earlier Turn's Subagents while no Turn is active. A Continuation settles like any Turn; one still open when the next Prompt is delivered settles first, interrupting any Provider work it owns before that Prompt begins.
_Avoid_: Synthetic turn, background turn, ghost turn

**Settle**:
The transition of a Turn or Activity into a terminal state — completed, failed, or interrupted — after which it accepts no further Provider output. A Session Settles in its own, reversible sense: set aside as done for now, by the user's say-so or on its own after long enough idle, and active again the moment it is prompted or the user unsettles it. Only the say-so is stored, as a marker stamped with the moment it was set. Settling on its own — **auto-settle**, governed by one Setting holding either how long being left alone has to be or the word that suspends it — is instead derived wherever Sessions are listed, from the Session's last activity: nothing is written down for it, no clock fires for it, and moving that Setting reclassifies every Session at once. Neither a Session nothing has moved since it was made nor one Suru cannot read auto-settles: the first has set nothing aside and the second has no activity Suru can see. Unsettling moves a Session's last activity to the moment the user reached for it, so auto-settle cannot put back what they just took off the shelf. Wherever Sessions are listed by liveness, the settled ones stand apart from the active ones.
_Avoid_: Finish, close, resolve; archive (for a settled Session)

**Working**:
The liveness of a Session that owes a Turn to an admitted Prompt, whose current Turn has not Settled, or whose surviving Subagents still work after that Turn Settles. Working begins the moment a Prompt is admitted to begin a Turn, before the Agent has been reached, is continuous across each of those boundaries, and applies at every depth of the Session tree. Interrupting a Session that is Working only for a Prompt it has not yet delivered **withdraws** that Prompt instead of stopping a Turn: the Prompt is cancelled, the Session stays as it is, and the client returns the text to the composer.
_Avoid_: Active, busy, running

**Usage**:
The record of the tokens a Turn consumed, kept in five parts — fresh input, cache reads, cache writes, output, and the reasoning within that output — with any part a Provider does not report simply absent, never guessed at zero. A Turn records its Usage the way it records when it began and Settled, and a failed or interrupted Turn keeps whatever Usage it accrued, because Usage answers what a Session has consumed rather than what it got for it. A Session's Usage is the sum over its Turns' Usage, and the total a surface shows for a Session includes its Subagent subtree, while each child Session keeps its own.
_Avoid_: Token count, consumption, spend

**Context Fill**:
The latest known number of tokens occupying a Session's context, expressed against its Model's context window when that capacity is known. It belongs to that Session alone, excludes its Subagents, and can decrease after compaction, unlike cumulative Usage; its denominator is the context window, not the Provider's compaction threshold.
_Avoid_: Session Usage, total tokens used, context remaining

**Cost**:
The dollar figure for recorded work, fixed when recorded and never restated against later prices; a Provider-reported figure outranks a Suru estimate, and unknown Cost remains absent rather than zero. Cost is the API-equivalent figure even where a subscription means nothing marginal was billed, with its origin stated by Cost Basis and the work it accounts for stated by Cost Coverage.
_Avoid_: Price (that is a rate), spend, billing

**Cost Coverage**:
The work a Cost accounts for, including whether it covers a Session's own work or also its descendants, so overlapping amounts contribute only once to a total. A total with known amounts and uncovered work retains those amounts and is marked partial; a whole-tree amount can cover descendants whose individual Costs remain unknown.
_Avoid_: Cost Basis (which describes origin), billing coverage

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
The Agent Selection a Provider declares for running Errands, chosen for cheapness and speed rather than capability, and resolved against that Provider's live Models each time an Errand runs, so a Model that has gone gives way to the Provider's default rather than failing the Errand. Whatever asks for an Errand decides which Provider runs it, and the Provider's own declaration then decides which Model: deriving a Title follows the Session's own Provider, and a Session that has selected no Provider runs no Errand at all. A Setting may pin one Agent Selection for every Session instead, with only explicitly chosen Model Options pinned and omitted ones following the selected Model's current defaults, which stands in front of the Provider's declaration rather than beside it — so the pinned Model is the one an Errand runs at, a Session that has selected no Provider is titled like any other, and the same resolution applies to the pin, a Model that has gone giving way to that Provider's default.
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

**Questionnaire**:
A Provider-native request for structured user input during a Turn, containing one or more Questions. Distinct from a permission approval or a question written in an Agent's ordinary prose.
_Avoid_: Question tool, input request

**Question**:
One item in a Questionnaire asking the user for input.
_Avoid_: Prompt, field

**Answer**:
The user's submitted response to a Questionnaire, distinct from a Prompt.
_Avoid_: Reply, response message

**Approval**:
A Provider-native request for the user's consent before an Agent's Tool may act during a Turn, with a subject naming what is asked — a Command, a File Change, a read, network access, a permission grant, or another Tool by name. Distinct from a Questionnaire, which asks for input rather than consent, and from a question written in an Agent's ordinary prose. An Approval is answered with a Decision, and an Approval the Provider settles itself under its own rules never reaches the user.
_Avoid_: Permission prompt, confirmation, consent request

**Decision**:
The user's response to an Approval: Accept, Accept for Session, Decline, or Decline and Interrupt. Accept for Session lets the same request pass unasked for the rest of the Session; Decline and Interrupt refuses and ends the Turn.
_Avoid_: Answer, verdict, response

**Approval Posture**:
The Provider-native permission configuration under which a Session's Agent acts: for each Provider, the native values that Provider offers for deciding which Tool uses need an Approval. A Session follows its Provider's Setting until the user overrides it, and an override is then the Session's own, surviving resume and restart until reset. A change takes effect at once where the Provider allows it and at the next Turn where it does not, and never decides an Approval already pending. Subagents act under the posture of the Session that spawned them.
_Avoid_: Permission mode, approval mode, trust level

**Subagent**:
An agent to which a Turn's Agent delegates work through its Provider, running its own conversation in its own Session — a child of the Session whose Turn spawned it. The parent's Transcript records each Subagent as an Activity of its own kind: one row naming the Subagent and what it was asked to do, wearing the usual Marker while it works and its outcome and duration once it settles, and standing — live or settled — as the way into the Subagent's Session. That row is all the parent's Transcript carries of it: the Subagent's work belongs to its own Transcript, never interleaved into the parent's. A Subagent may outlive the Turn that spawned it; while any Subagent still works the Session is still Working, and output one provokes after its Turn Settled lands in whatever Turn is active or begins a Continuation. Subagents may spawn Subagents, each recorded the same way one level down. Interrupting a Session whose Subagents still work stops them along with whatever else the Session is doing, and a single Subagent may be stopped on its own where its Provider allows it; neither asks before acting, as interrupting never does.
_Avoid_: Task, child agent, background agent, worker

**Skill**:
A Provider-offered body of task-specific guidance that an Agent may use and a user may explicitly select for a Prompt. Suru identifies an offered Skill to clients by an opaque identity and safe presentation metadata, leaving its native path or command inside the Provider. Distinct from a Tool, which performs work; a Skill guides how work is approached.
_Avoid_: Command, prompt template

**Skill Invocation**:
A user's explicit selection of a Skill for one Prompt, written as `$skill-name` in that Prompt and carried as a typed binding beside the original text. It remains bound to that Provider's Skill even when Providers differ in how they receive it; an unbound `$token` is ordinary text. When creating a new Worktree, an explicit selection follows a unique matching Skill name into the destination Catalog automatically; a missing or ambiguous match prevents the Prompt from being admitted.
_Avoid_: Skill mention, skill command

**Skill Catalog**:
The current set of user-invocable Skills a Provider offers in an Execution Directory, together with the Provider's effective Skill Invocation limits and Prompt delivery support. Before a Session's first Turn it follows the Agent Selection, afterwards it follows the Session's fixed Provider, and its contents may change when the Provider's environment changes.
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
The ordered, user-visible history of a Session: its Messages and Activities in presentation order. A Prompt admitted to begin a Turn but not yet delivered is drawn by every client in the Transcript's position as the user Message it will become, so no reader waits on the Agent to see what was asked.
_Avoid_: History, log, conversation

**Working Indicator**:
The transient presentation immediately after a Session's latest Transcript row while that Session is Working. It distinguishes work belonging to the current Turn from waiting on surviving Subagents, and carries the work's elapsed time and interruption guidance without becoming a Message or Activity.
_Avoid_: Active text, status text, loading row

**Session Content Column**:
The main working column of an open Session: its Transcript and Working Indicator, queued Prompts, latest-position affordance, composer extensions, composer, and composer footer. The Session header and the Landing sit outside it.
_Avoid_: Conversation column, transcript column

**Sidebar**:
The collapsible column a client shows beside its main view, listing Sessions with the active apart from the settled, searched by Title, and scoped to one Workspace or all of them. Whether it begins shown, how wide it begins, and how wide its scope begins, are Settings; showing or hiding it afterwards, and dragging its **edge** — the rule between it and the main view, painted in the Theme's focus color for as long as it is held — are the reader's own view state, kept only while the client runs. The Sidebar is never narrower than its own floor and never leaves the main view less than its floor, so a width the reader asks for is drawn as wide as the terminal allows and restored whole when the terminal grows; a terminal too narrow for both it and the main view keeps the main view. An active row carries its Session's Standing twice over, from one reading: as the color of the Rail down its left, and as the word in its **right slot**. Its third line presents the Worktree's current Checkout State, appends **(worktree)** after a linked Worktree's branch name while leaving the main Worktree implicit, marks unavailable state explicitly, and stays blank for directories outside supported source control. While the Session is Working the slot reads **Working** with the duration the work has been running; a Standing with no word to say leaves the slot to the compact time since the Session last moved. The settled Sessions stand on a **shelf** below a **divider**: a rule the Sidebar draws only where something is settled, closing the active list and naming what the rows beneath it are. Shelf rows are slim where active rows are not, because settled work is history a reader keeps in view rather than work they are choosing between. The shelf is bounded: it opens on its first rows and the rest of the tail stands behind an affordance the reader asks for, batch by batch, until the shelf is whole. A **search box** at the top of the Sidebar narrows it by Title, and while it carries a query both shelves stand down: the Sidebar answers with one flat list of results, in the order the shelves would have drawn them and each keeping the shape its shelf gives it. Under it a **selector** says which Workspace the Sidebar is answering for and opens the ways into the reader's work: Everywhere first, then all of the Outlook's Workspaces, then every Workspace it has Sessions in and the one the client itself runs in, whether or not there is work there yet. Choosing a Workspace narrows both shelves and the results a query answers with; the Sidebar goes on listing the Outlook's whole body of work either way, because the entries are read off that listing and narrowing what it asks for would take the other Workspaces off the selector along with their Sessions. Choosing Everywhere widens the listing past the Outlook rather than narrowing it, and the selector offers no Workspace of another Server: a Workspace elsewhere is reached by turning the Outlook. Beside the selector an **add-Workspace affordance** opens a **path entry**, which stands in place of the list and takes what they type: a path they give relative is read from the current Execution Directory, and the owning Server resolves the directory it names into an Execution Directory and its Workspace. The directory becomes where the next Session works, while its Workspace becomes the client's current Workspace and what the selector narrows to; a bare Repository root selects only the Workspace and requires a working-copy choice before execution. A path standing at a file or at nothing is refused where the reader can see it and nothing moves. Where the open Session has a row in the list as presently drawn, that row alone carries the Sidebar's open-Session **highlight**: its Title drawn in the Theme's accent color, and nothing else about the row repainted, so the highlight never hides the Rail or the row's other readings; the Landing, an open Session narrowed or searched out of the list, and an open Subagent leave no row highlighted. The highlight says what the main view is for rather than whether its content has finished loading, where the keys are, or whether a Session is Working: opening a Session moves the highlight, clears the old content, and gives its composer the keys at once. Its header, Transcript, and other Session-derived content stand blank until they arrive; if that takes longer than 300 milliseconds, **Loading** appears in the Working Indicator's shimmer style until they do. The composer takes and keeps a draft under that Session from the first moment, but delivers no Prompt and offers no other Session-dependent act until the Session arrives. Failure leaves that Session open with its highlight and draft intact, replaces Loading immediately with a client-local, transcript-shaped **Error: Could not load Session:** row in the Theme's error color, and keeps delivery disabled; the error resembles Transcript content but never becomes part of the Transcript. Reaching the failed Session again through its Sidebar row retries it, clearing the error at once and beginning another quiet 300 milliseconds; without that row there is no retry, and the failed view stands until the reader goes elsewhere. A newer route — another Session, the Landing, or another Workspace — cancels an opening still under way and no late answer may pull the client back. While the reader drives the Sidebar, a separate, transient **row focus** walks its controls and rows in their drawn order without changing the open Session. It is painted in the Theme's focus color only while the Sidebar owns the keys, beginning on the open Session where its row is drawn and on the Workspace selector otherwise; another surface taking the keys hides it until they return, and leaving the Sidebar gives it up. Up and Down wrap through every focusable control and readable Session row in their drawn order; a moving listing keeps focus on the same entry where it survives and otherwise carries it to the nearest one left. Row focus is painted whole, and a Rail is painted over its left column, so a row that is focused, open, and Working still says all three. Enter acts on what row focus stands over. A Session the client cannot read keeps a subdued row whose Title is followed by **[unreadable]**, a marker truncation never takes away; keyboard navigation passes it over, pressing it does nothing, and its context menu offers deletion but never settling. The other Session rows answer a pointer as readily as the keys: pressing one performs the same optimistic opening as Enter, without raising row focus where the Sidebar does not own the keys; a row asked for its **context menu** is offered the shelf it is not on — set aside, or brought back — along with deleting the Session, which asks again before it acts. A Sidebar on screen is as true as the server without the reader asking: every change the session-catalog stream reports — a Session made, retitled, deleted, set aside, brought back, its latest Turn begun or settled, or a whole catalog reconciled after a reconnection — is taken as it happens, and the Sidebar asks for its listing again to carry what the change itself does not say. Catching up is not the reader looking again, so it leaves the shelf as deep as they walked it and whatever they had opened standing.
_Avoid_: Panel, drawer, session list, nav; archive or section (for the settled shelf)

**Rail**:
The one-column stripe down the left of an active Sidebar row, spanning every line of the row, painted in the Theme's feedback color for the Session's Standing and absent where the Standing has nothing to say. Settled shelf rows carry none.
_Avoid_: Indicator bar, status stripe, gutter

**Standing**:
The one reading a listed Session presents about its work, from which both its Rail's color and its right slot's word derive: Needs Intervention, Working, Failed, Done, or nothing, in that order of precedence. Failed and Done hold only for a Settled latest Turn that no Client has Viewed since; an Interrupted Turn leaves nothing. Working maps to the Theme's info color, Needs Intervention to warning, Failed to error, and Done to success. Needs Intervention marks a Session with a pending Questionnaire awaiting an Answer or a pending Approval awaiting a Decision, whether its own or one belonging to a Subagent at any depth; the Answer or Decision belongs in the owning Session.
_Avoid_: Status (taken by Idle and Active), attention, state, condition

**Viewed**:
The Server's record of the last moment any Client had a Session open in its main view, reported by that Client when it opens the Session and again when a Turn Settles while it is open. It is one fact about the Session rather than about any one Client, so one Client Viewing a Session clears its Failed or Done Standing for every Client. A Session on screen counts as Viewed whether or not the reader is looking at the terminal.
_Avoid_: Read, seen, acknowledged

**Subagent Picker**:
The docked list a client opens over the composer to browse the open Session's working Subagents, drawn as the tree they spawned in. It opens only while there is something to browse, so the key that opens it stays inert otherwise; moving through it and choosing an entry opens that Subagent's Session, a working entry may be stopped from its row, and closing it lands back where it opened. Settled Subagents are not its concern — they are reached from their rows in the Transcript.
_Avoid_: Agent panel, roster, subagent list

**Workspace Picker**:
The centered, searchable list a Client opens to switch among the Workspaces its Sessions belong to and its current Workspace. Choosing one opens the Landing with that Client's last Execution Directory for the Workspace on its Server, defaulting to the main Worktree when none is remembered or requiring a Worktree choice for a bare Repository; the open Session keeps its own work and the Sidebar keeps its chosen scope.
_Avoid_: Project picker, project list, workspace switcher

**Worktree Selector**:
The Landing control that chooses an existing Worktree for the next Session or asks Suru to create a new one on first Prompt submission. It names the current location above its list without offering it as a choice, then offers creating a new Worktree first and selected, then every Worktree of the Repository with its Checkout State and its location: the leaf name alone for a Worktree Suru manages, the path beneath the Workspace's main root for one within it, and the whole path otherwise. The Worktree the reader is already in is marked among them, and choosing it keeps the location as it stands. Choosing another existing Worktree or creating a new one starts at its root; a subdirectory is reached through the Sidebar's path entry instead.
_Avoid_: Workspace Picker, branch picker

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

**Server**:
The long-lived Suru process that owns a user's Sessions, Server Settings, and Provider work on one machine, one per Channel, which Clients attach to rather than doing that work themselves.
_Avoid_: Daemon, backend, host

**Client**:
A user-facing Suru surface — today the TUI — that attaches to the Server on its own machine and presents that Server's work. A Client never speaks to another machine's Server directly; anything remote it sees, it sees through its own Server.
_Avoid_: Frontend, terminal, UI process

**Serving**:
The state in which a Server accepts connections from other machines' Servers, off unless its user turns it on as a Setting, and never extended to the machine's own Clients — they attach the way they always have.
_Avoid_: Hosting, remote mode, exposing

**Invite**:
The one-time pasteable string a Serving Server issues so another machine's Server may form a Pairing with it: the addresses its user chose to offer, the Server's identity, and a token spent by its first redemption, dead after a short while unused, and superseded by the next Invite either way.
_Avoid_: Connect string, join code, ticket

**Pairing**:
The durable relationship formed when one Server redeems another's Invite: each side holds the other's identity and trusts nothing else, so the two find each other again on their own until one side removes the other. A Pairing is one-way — the redeeming Server reaches into the Serving one, never the reverse — and a second Pairing in the opposite direction is its own relationship.
_Avoid_: Link, tunnel, connection (for the relationship itself)

**Remote**:
A paired Serving Server as the redeeming side knows it: carrying a name its user gave it — offered from the machine's own hostname, theirs to change — reached only through the local Server, and offering its own Sessions and Workspaces for a Client to work in. What a Remote's user does on their own machine is none of the local side's business; a Remote shares its work, not its administration.
_Avoid_: Remote server, host, upstream

**Peer**:
A paired redeeming Server as the Serving side knows it: an entry its user can list and remove, and removing it ends the Pairing.
_Avoid_: Authorized client, key entry

**Outlook**:
The Server whose world a Client is presently looking into: its own machine's Server unless the user has turned it toward a Remote. A Client holds one Outlook at a time, and everything it presents that acts or begins — the Landing, the Skill Catalog, the pickers of Agents and Workspaces, the Session it offers to begin — answers for that Server alone. A listing of Sessions is the one thing that may range wider, when its scope is Everywhere; opening a Session there turns the Outlook toward that Session's Origin, and the Outlook's current Workspace becomes the one the Client remembered for it or, where it remembered none, the opened Session's own. The user may also turn it deliberately, which resolves the Remote's own working directory instead. A Client always shows which Remote its Outlook is turned toward, so a turn is never silent. A Remote whose Pairing ends while the Outlook is turned toward it turns the Outlook back to the Client's own Server, with an error where the reader can see it.
_Avoid_: Scope (the Sidebar's Workspace narrowing already owns it), view, context, focus

**Origin**:
The Server a Session lives on: the Client's own machine's Server, or a Remote known by its name. A Session's identity is only unique within its Origin, so every Session a Client can reach is held as identity and Origin together, and everything done to a Session — opening, prompting, settling, deleting — goes to its Origin however the Client came by the row. Wherever Sessions from more than one Server stand together, a row whose Origin is a Remote carries that Remote's name, and an open Session's header carries it likewise; a Session of the Client's own Server carries nothing, because the ordinary case stays quiet.
_Avoid_: Source, host, server tag, home

**Everywhere**:
The scope under which a listing of Sessions ranges over every Server the Client can reach — its own and every paired Remote, whether or not that Remote currently answers — across all their Workspaces. It stands first in the Sidebar's selector, is one of the states the Session picker cycles through, and may be named by the Setting that gives the Sidebar its starting scope. A Remote that has paired into this machine as a Peer is not reached by it, because a Pairing is one-way; a machine the reader wants to see is one they pair with from their side. Everywhere lists in the one flat order the shelves always draw, by recency as each Server reports it, without grouping by Server and without correcting one machine's clock against another's. While it is chosen every Remote is kept in view, whether or not the surface listing it is on screen, so the listing is as true as its Servers the moment it is shown. A Remote that stops answering keeps the rows it last gave, dimmed, and stands as a slim subdued row of its own at the foot of the active list marked **[unreachable]** — a row that offers to try again and that leaves as soon as the Remote answers; one whose Pairing has ended takes its rows with it. Everywhere changes what is listed and nothing about where work begins: choosing it leaves the Outlook where it stands, and opening a row turns the Outlook toward that row's Origin with the scope still chosen.
_Avoid_: All servers, all machines, fleet, merged view, global

**Channel**:
The build variant (release or a development channel) whose Sessions and runtime state are kept separate. Which Worktrees a Repository has is no concern of the Channel: Suru manages its Worktrees in one place, and Channels that select the same Worktree share its working-copy files.
_Avoid_: Environment, profile

**Log**:
The operator-facing diagnostic record a Suru process writes about its own run. A Log is about Suru's behavior, never about conversation content — the user-visible history of a Session is its Transcript.
_Avoid_: Diagnostics, telemetry, trace

**Text Selection**:
The run of text a reader marks on a client's screen, by dragging across it, by double-clicking a word, or by triple-clicking a Line, so that the text behind it may be copied as it was written rather than as it was wrapped or decorated to fit the screen. It is a fact about the client's screen rather than the Session: it lives in no Transcript, belongs to the one surface it was begun on, follows that surface's content as it scrolls, and is gone the moment that content is laid out afresh — the composer's content being the draft itself, so a selection there outlives a resize and goes only when the draft goes. A client holds at most one at a time, so marking text anywhere lets go of whatever was marked before. In the composer it is also made and extended from the keyboard, with Shift held against the keys that move the cursor, and it is the run the next edit acts on: typing or pasting replaces it, Backspace and Delete remove it, and cutting copies it and removes it in one act. Distinct from an Agent Selection, from the row focus a list keeps, and from a Fold's disclosure.
_Avoid_: Selection (bare), highlight, mark, mouse selection

**Line**:
One line of a surface's text as it was written, which the Transcript may paint across several Rows to fit its width. A triple-click marks a whole Line.
_Avoid_: Logical line, source line, paragraph

**Row**:
One painted screen row of a surface. A Row is a fragment of a Line, or a Line entire on a surface that never wraps.
_Avoid_: Visual line, screen line, wrapped line

**Clipboard**:
Where content taken from Suru is offered for pasting elsewhere: formatted Markdown carries both formatting and its Markdown text alternative, while other copies carry plain text. A formatted copy prefers the machine's Clipboard and falls back to the terminal's when needed; plain-text copies reach both, and either kind also fills the machine's primary selection where one exists.
_Avoid_: Host clipboard, system clipboard, OSC 52 (for the destination itself)

**Notice**:
A transient, one-line message a client shows about Suru's own behavior rather than about a Session. The reader's next interaction dismisses that Notice and it does not return, while a distinct runtime condition may raise a new Notice; each points at the Log rather than carrying the detail itself.
_Avoid_: Banner, toast, alert, diagnostic

**Theme**:
The named set of colors a Client paints its surfaces with, chosen as a Setting, as opposed to the shapes and layout of what it paints. Every Theme but System brings its own background; Suru ships a set of Themes and a user may supply more.
_Avoid_: Color scheme, skin, palette (for the user-facing choice)

**System**:
The Theme that paints with the terminal's own colors and leaves its background to the terminal, so Suru looks like whatever the terminal already is. When the terminal reports that its colors changed, a Client using System reads them again and repaints without restarting. It is the Theme every Client starts with.
_Avoid_: Default theme, terminal theme, no theme

**Code Block**:
A fenced region in an agent's answer or Reasoning; answer blocks use language syntax colors when recognized and plain code color otherwise, while Reasoning blocks stay plain and subdued. Shell output and file changes are not Code Blocks.
_Avoid_: Snippet, code fence, highlighted code

**Chrome**:
What a client paints around a Message's or Activity's words to show their structure, such as list bullets, quote bars, Table borders, and fence labels. Chrome belongs to the screen rather than the content, so a Text Selection copies the words and never the Chrome.
_Avoid_: Decoration, markers, prefix

**Table**:
A grid an agent authors in a Message or Reasoning, painted in bordered columns that fit the Transcript's width and copied as the grid it was written as, so a destination that understands tables receives one.
_Avoid_: Pipe table, grid

**Setting**:
One user-tunable value governing Suru's behavior, carrying a built-in default that applies whenever no Config Document pins it. Every Setting declares a scope: a **Client Setting** governs a client's presentation, and a machine-local Config Document may one day overlay it, while a **Server Setting** governs server or Provider behavior and follows only the server's own Config Documents. Every Setting also declares what it accepts: a **Fixed Setting** — almost all of them — accepts a set of values named up front, which is what lets a reader cycle one through them and lets Suru say exactly what to type where a value is rejected, while an **Open Setting** holds something Suru only discovers while running, such as an Agent Selection, and so names the values it can and describes the rest. Every Setting also declares the **group** it keeps company with, which is the whole of what it says about its own presentation: a client maps a group to the tab that lists it, and the group of **Experimental** Settings holds the ones still finding their shape, so a reader meets them knowing as much. Distinct from a Model Option, which is Provider-advertised rather than user-authored.
_Avoid_: Option, preference, config value

**Config Document**:
A file in which a user pins Settings. Config Documents stack in a fixed precedence order, and an edit Suru makes to one changes only the value it targets, leaving the rest of the document — its ordering, spacing, and comments — untouched.
_Avoid_: Settings file, preferences file, config
