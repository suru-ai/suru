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
A workspace for conversation between a user and an agent. A Session is independently addressable, may be viewed from multiple clients, and may exist before an agent is selected. Its first Turn binds it to that Agent's Provider; later Agent Selections may change the Model and Model Options, but not the Provider. A Subagent's Session is a child of the Session whose Turn spawned it: viewed, streamed, and stored like any other, to any depth of children of its own, but reachable only from within the tree its top-level Session heads — never listed where Sessions are listed, never offered a Prompt, and deleted along with the parent whose deletion it shares.
_Avoid_: Chat, thread, conversation

**Title**:
The short line by which a Session is known both while viewing it and wherever Sessions are listed, and the text a reader searches those listings by. A Title begins as the Session's first Prompt trimmed of the space around it — a real Title rather than a placeholder — and Suru replaces it once it has derived a better one through an Errand. Derivation is attempted once, when the first Prompt is admitted, and never again: a Session whose derivation was skipped, failed, or abandoned keeps the Title its Prompt gave it for good. A derived Title only replaces the Title it was derived from, so a Title since set by other means stands. A Setting decides whether Suru derives Titles at all, and which Agent Selection does the deriving; the same Setting governs the branch names Suru derives for Managed Worktrees alongside them.
_Avoid_: Name, subject, summary

**Icon**:
One Nerd Font glyph standing for a Session beside its Title, or for a Workspace wherever it is named, chosen from the Icon Catalog and carried as a typed property rather than written into any text, so searching a listing matches the words a reader remembers rather than the glyph in front of them. A Session's Icon is derived with its Title in the same Errand; a Workspace's is derived from its name and README once it has a Session to lend the Errand a Provider, and attempted again with each new Session until one lands. Either may also be chosen by the user, and a chosen Icon stands: derivation only ever fills an absence. Either may have none, which every surface draws as readily as it draws one; a Workspace without one is named beside the plain folder glyph instead. Whether Icons are drawn at all is the one Setting governing every Nerd Font glyph Suru shows, so hiding them is presentation and nothing else: an Icon goes on being derived, stored, and carried to every client. The fixed glyphs beside a Provider, a branch, or a folder are not Icons in this sense.
_Avoid_: Emoji, glyph — which a Marker and a Spinner already claim — avatar, badge

**Icon Catalog**:
The set of Nerd Font glyphs Suru itself knows by name, from which every Icon is chosen, whether by an Errand or by the user. An Icon is remembered by its Catalog name rather than its codepoint, so a glyph the Catalog no longer carries is drawn as no Icon at all.
_Avoid_: Glyph table, icon list, nerd font names

**Landing**:
The view a client shows when no Session is open, carrying the Agent Selection a new Session will begin from, its intended execution location, and the composer its first Prompt is written in. Beneath the composer it names the Workspace by its presented root, followed by the selected Worktree's Checkout State as a Sidebar row draws it and, when working in a subdirectory, the path relative to that Worktree's root; the user may choose an existing Worktree or ask Suru to create one on submit, with that pending intent shown here and its branch, starting commit, and location managed by Suru.
_Avoid_: Home, launch view, start screen, welcome screen

**Provisional Session**:
The Session view a client shows from the moment the Landing's first Prompt is submitted until the Server answers with the Session it made, drawn from what the client already knows — the Prompt as a user Message, the Title the Prompt gives, the Execution Directory, the Agent Selection, and the Working Indicator — before any of it is confirmed. It is that client's own claim, never listed in the Sidebar, and replaced in place by the real Session when it arrives; its Working Indicator carries no elapsed time, because only the Server knows when Working began. When a new Worktree is requested, the view appears immediately and its indicator says **Creating worktree** without elapsed time through checkout creation and destination Skill discovery, then uses the ordinary Session-start indicator. Worktree preparation failure keeps this view and its Prompt in place for retry, reusing any retained Worktree. Interrupting Worktree preparation prevents Prompt admission and returns its text to the composer; any ongoing checkout operation may finish and its Worktree is retained. Its composer takes a draft but delivers nothing until the Session arrives, and a newer route abandons it without pulling the client back. If the Server refuses, the view stands with the user Message in place and a client-local, transcript-shaped **Error: Could not create Session:** row where the Working Indicator was, saying that Enter retries and that a new prompt may be typed instead: an empty submit retries the same Prompt, and text submitted replaces it as a new Prompt. Leaving a failed Provisional Session discards it and keeps its text as the Landing's draft.
_Avoid_: Optimistic session, pending session, draft session, creating state

**Workspace**:
The working context that groups Sessions on one Server by their shared Repository, including its Worktrees and their subdirectories, or by an individual directory outside source control. Separate clones and nested Repositories are separate Workspaces; a Workspace is presented by its main root, bare root, or repository metadata location when the main root is unknown, without making that label its identity or its Sessions' Execution Directory.
_Avoid_: Project, working directory, location

**Description**:
A sentence or two saying what a Workspace is for, kept beside its Icon so that a reader choosing among Workspaces, or a Sidekick asked to find the right one, can tell them apart by more than a name. It is derived in the same Errand as the Workspace's Icon, from the same name and README, and like the Icon it only ever fills an absence: each of the two is attempted until it lands, neither waits on the other, and a Description the user or a Sidekick has set stands. A Workspace may have none.
_Avoid_: Summary, about, blurb, notes

**Repository**:
The local source-controlled body of work a Workspace belongs to, including its related working copies. Sharing a remote address does not make separate clones the same Repository.
_Avoid_: Remote, origin, project

**Worktree**:
A Git Repository's working copy, either its main working copy or a linked one elsewhere on the same Server, shared by any Sessions that work within it. Every Worktree belongs to its Repository's Workspace, including those created outside Suru; settling a Session leaves it in place, while deleting the last Session that references a Managed Worktree can make it Reclaimable.
_Avoid_: Workspace (for an individual linked working copy)

**Managed Worktree**:
A linked Worktree that Suru created for a Session, living under the Repository's managed container on a branch Suru named and started from the commit the Session was created against. Its location and branch are first named from a few words of that Session's first Prompt, leaving out its Skill Invocations, and its location keeps that name for good. Suru then replaces the branch's name once, with a better one derived in the same Errand as the Session's Title, and only while the Worktree is still on that branch and the branch has not yet been pushed; a derivation that is off, fails, or comes too late leaves the first name standing. A name already taken gains a number rather than displacing whatever holds it. Only Managed Worktrees are ever Reclaimed; a linked Worktree the user created elsewhere is theirs to remove.
_Avoid_: temporary worktree, auto worktree, sandbox

**Reclaimable**:
A Managed Worktree eligible for Reclaim because no Session or unfinished preparation references it, every Session that references it has been inactive past the configured threshold, or its failed preparation is older than that threshold. Working or Monitoring Sessions, changes Git would need force to discard, initialized submodules, and Git locks Suru did not place make it ineligible.

**Reclaim**:
The Server's unattended removal of a Reclaimable Managed Worktree, distinct from explicit removal that the user asks for and confirms. It retains the branch unless fully merged into its recorded base, and leaves affected Sessions' histories and settlement unchanged so their next Prompt can recover the Worktree.
_Avoid_: cleanup, prune, sweep, garbage collection, expire, retire

**Execution Directory**:
The directory in which a Session's Agent works, which may be a Worktree root or a subdirectory and is fixed from its first Turn onward. Grouping a Session under its Workspace preserves this directory, including for Sessions that already exist; working elsewhere begins another Session.
_Avoid_: Workspace root, project directory

**Checkout State**:
The current branch or detached commit of a Worktree, shared by the Sessions that work within it rather than remembered separately for each Session. A Sidebar row presents this current state with the main Worktree implicit, distinguishes a linked Worktree, and explicitly marks the Worktree unavailable when its state cannot be read.
_Avoid_: Session branch, original branch

**Turn**:
A unit of work that ordinarily begins when a Prompt is delivered while a Session is idle — or, in a Subagent's Session, a Delegation — and includes the resulting Agent activity. Two kinds of Turn begin otherwise: one begun by the user's request for a Compaction, which draws no user Message and holds only that Compaction — or, where its Provider reported none, at most the Error saying why it failed — and a Continuation, which begins with nothing asked at all. A Turn retains its effective Agent identity and may accept delivered steer Prompts, or in a Subagent's Session steer Delegations, without beginning another Turn. A Turn records when it began and when it Settled, so any surface reading it can state how long it worked. A Turn's Settle is the Provider's own boundary — or, when a Compaction begun in the Turn outlasts that boundary, the Compaction's settle — so a Turn may Settle while Subagents it spawned work on; the Session is not idle again until they too have settled.
_Avoid_: Request, exchange

**Continuation**:
The one kind of Turn that begins with nothing asked of the Agent — no Prompt, no Delegation, no requested Compaction: the Provider does more work after a prior Turn Settled, including work provoked by a Watch, a Subagent, or a Subagent Report, or Suru receives output owed to an earlier Turn's Subagents while no Turn is active. A settled Subagent woken by its own Watch works on in a Continuation of its own Session rather than as a new Subagent. A Continuation settles like any Turn; one still open when the next Prompt or Delegation is delivered to begin a Turn settles first, interrupting any Provider work it owns before that Turn begins.
_Avoid_: Synthetic turn, background turn, ghost turn

**Settle**:
The transition of a Turn or Activity into a terminal state — completed, failed, or interrupted — after which it accepts no further Provider output. A Turn still open when Suru stops Settles as failed at the next start, at the moment of its last output, because nothing survives a stop to finish it; a Subagent's row in its parent's Transcript Settles with it. An Activity still running when its Turn Settles Settles with it: interrupted if the Turn was interrupted, since someone asked the work to stop, and failed otherwise. An Activity is failed on its own only when its Provider reports that it failed; an exit status is only a Command's exit code, never the evidence for how it settled. A Session Settles in its own, reversible sense: set aside as done for now, by the user's say-so or on its own after long enough idle, and active again the moment it is prompted or the user unsettles it. Only the say-so is stored, as a marker stamped with the moment it was set. Settling on its own — **auto-settle**, governed by one Setting holding either how long being left alone has to be or the word that suspends it — is instead derived wherever Sessions are listed, from the Session's last activity: nothing is written down for it, no clock fires for it, and moving that Setting reclassifies every Session at once. A Session that is Working or Monitoring never auto-settles, since it is not yet done. Neither a Session nothing has moved since it was made nor one Suru cannot read auto-settles: the first has set nothing aside and the second has no activity Suru can see. Unsettling moves a Session's last activity to the moment the user reached for it, so auto-settle cannot put back what they just took off the shelf. Wherever Sessions are listed by liveness, the settled ones stand apart from the active ones.
_Avoid_: Finish, close, resolve; archive (for a settled Session)

**Working**:
The liveness of a Session that owes a Turn to an admitted Prompt, whose current Turn has not Settled, or whose surviving Subagents still work after that Turn Settles. Working begins the moment a Prompt is admitted to begin a Turn, before the Agent has been reached, is continuous across each of those boundaries, and applies at every depth of the Session tree. Interrupting a Session that is Working only for a Prompt it has not yet delivered **withdraws** that Prompt instead of stopping a Turn: the Prompt is cancelled, the Session stays as it is, and the client returns the text to the composer. A Session whose only live work is Watches is Monitoring, not Working.
_Avoid_: Active, busy, running

**Monitoring**:
The liveness of a Session that is not Working but still has live Watches, any of which may wake its Agent into a Continuation. Like Working it applies at every depth of the Session tree, so a Session is Monitoring when a Watch anywhere beneath it is live and nothing in its tree is Working. Monitoring breaks Working's continuity: it counts its own time from when it began, and Working after a Watch wakes the Agent counts afresh. A Watch that never ends leaves its Session Monitoring until the Session is interrupted or its Provider process ends; interrupting a Monitoring Session stops every Watch in its subtree — not those of the Sessions above it — without asking and leaves it idle, with no Turn to Settle. Nothing survives a Server stop to keep a Session Monitoring, so none is Monitoring when Suru starts.
_Avoid_: Waiting, backgrounded, Working (for this state)

**Watch**:
Provider-run work the Agent left running past its Turn — a background shell, a monitor — whose completion or report may wake the Agent into a Continuation. A Watch belongs to the Session whose Agent started it, including a Subagent's Session after that Subagent settles, and it is that Agent a Watch wakes. A Watch has no conversation of its own, and the Command that started it already stands in the Transcript. Delegated work that runs an Agent of its own is never a Watch: a Subagent keeps its Session Working.
_Avoid_: Background task, background Command, job, monitor (for the general concept)

**Watch Outcome**:
The Activity recording how a Watch settled — completed, failed, or stopped — in the words its Provider gives, placed in the Turn its settling woke the Agent into. A Watch whose settling wakes nothing, such as one stopped by an interrupt or lost with its Provider process, records none, and a Watch's reports short of settling are never recorded.
_Avoid_: Task notification, background result, wake

**Usage**:
The record of the tokens a Turn consumed, kept in five parts — fresh input, cache reads, cache writes, output, and the reasoning within that output — with any part a Provider does not report simply absent, never guessed at zero. A Turn records its Usage the way it records when it began and Settled, and a failed or interrupted Turn keeps whatever Usage it accrued, because Usage answers what a Session has consumed rather than what it got for it. A Session's Usage is the sum over its Turns' Usage, and the total a surface shows for a Session includes its Subagent subtree, while each child Session keeps its own.
_Avoid_: Token count, consumption, spend

**Context Fill**:
The latest known number of tokens occupying a Session's context, expressed against its Model's context window when that capacity is known. It belongs to that Session alone, excludes its Subagents, and can decrease after a Compaction, unlike cumulative Usage; its denominator is the context window, not the Provider's compaction threshold.
_Avoid_: Session Usage, total tokens used, context remaining

**Context Breakdown**:
What occupies a Session's context, as its Provider attributes it when asked: the Context Fill it measures, split by source — the system prompt, Tool definitions, instruction files, Skills, Agents, the conversation — with the items behind a source where the Provider names them, and the part of the window it holds back from the Agent where it says. It is read on request and never stored, so it describes the context only as it was when asked. A Provider that measures its Context Fill without attributing it offers none, and neither does a Session whose Provider is not running, or whose conversation rides an ancestor's Provider, which can describe only its own.
_Avoid_: Context usage, context attribution

**Compaction**:
The Activity recording one occasion on which a Provider replaced what its Agent remembers of a Session with a summary, to free room in its context. A Compaction is **automatic** when the Provider chose to do it, inside whatever Turn it fell in, and **manual** when the user asked for it, which only an idle Session accepts and which begins a Turn of its own. It is Active while the Provider summarises and Settles like any Activity. Each attempt is its own Compaction, so one that fails and is tried again leaves two. A manual Compaction's Turn Settles as its Compaction does, and accepts no steer: a Prompt sent to steer while it runs is held to begin the next Turn once the Compaction completes, and is withdrawn, its text returned to whoever wrote it, if the Turn settles any other way. A queued Prompt is not held: it waits in the queue as behind any Turn, and cannot be promoted to steer while the Compaction runs. An automatic Compaction that fails leaves its Turn to Settle as the Provider says, and one that outlasts the Provider's own turn holds its Turn open until it settles; a Prompt delivered meanwhile steers that Turn. It carries nothing guessed: which of the two it was, the summary where its Provider gives one, which a Transcript shows behind a Fold, and the Context Fill before and after, as the Provider reports them or else as Suru last read it before the Compaction began and first read it after. An interrupted Compaction left the Agent's context as it was, so it carries no Context Fill and no summary. A manual Compaction may carry the user's **instructions** on what the summary should keep, where the Provider takes them; where it does not, the request is refused rather than the instructions dropped. A Compaction changes only what the Agent remembers: the Transcript keeps everything before it. It belongs to the Session whose context was compacted, so a Subagent's Compaction stands in the Subagent's own Transcript and never in its parent's.
_Avoid_: Summarization, compression, truncation, context reset

**Cost**:
The dollar figure for recorded work, fixed when recorded and never restated against later prices; a Provider-reported figure outranks a Suru estimate, and unknown Cost remains absent rather than zero. Cost is the API-equivalent figure even where a subscription means nothing marginal was billed, with its origin stated by Cost Basis and the work it accounts for stated by Cost Coverage. A surface states two readings for a Session. Its **own Cost** is the Provider's account of that Session's own conversation. Its **tree Cost** is that own Cost with every descendant's rolled up beneath it, native and brokered, to any depth, and never anything above it. A Claude Session's own Cost includes the native Subagents Claude ran inside its process, because Claude reports no split and Suru invents none; a brokered Subagent's Cost is never in its caller's own Cost.
_Avoid_: Price (that is a rate), spend, billing, self cost, parent-only cost, subtree total

**Cost Coverage**:
The work a Cost accounts for, including whether it covers a Session's own work or also its descendants, so overlapping amounts contribute only once to a total. A total with known amounts and uncovered work retains those amounts and is marked partial; a whole-tree amount can cover descendants whose individual Costs remain unknown. A whole-tree amount covers only the descendants its Provider ran itself: native Subagents, reached without crossing a brokered Session. It never covers a brokered Subagent, whose own Provider actor meters that Subagent's work separately, so a brokered Subagent's Costs are added beneath any ancestor's whole-tree amount.
_Avoid_: Cost Basis (which describes origin), billing coverage

**Cost Basis**:
Where a Cost came from: **Reported** when the Provider itself stated the figure, **Estimated** when Suru computed it from a rate table. Basis records who computed the number rather than whether money changed hands — a Reported Cost under a subscription may still have billed nothing.
_Avoid_: Cost source, cost type

**Prompt**:
Input submitted for delivery to an agent: the user's own, or a Sidekick's sent on the user's behalf. A delivered Prompt becomes either the user Message that begins a Turn or a later user Message that steers its active Turn. A Prompt a Sidekick sent says so as a typed property naming the Sidekick's Session, and its Message is drawn apart from one the user wrote and leads back to that Session, so a reader always knows which words were theirs.
_Avoid_: Request, draft, message

**Attachment**:
Media a user places on a Prompt beside its text — an image pasted from the clipboard being the first kind — that stands in the Prompt's text as a numbered label such as `[Image 1]`, carried as a typed binding beside the text as a Skill Invocation is, and travels with the Prompt whether that Prompt begins a Turn or steers one. An Attachment belongs to the user Message its Prompt becomes for as long as the Session does, so every client viewing the Session can present it.
_Avoid_: File, upload, media, image (for the concept), attachment (for a Client joining a Session)

**Delegation**:
The instruction a Turn's Agent gives a Subagent, through its Provider or through the Broker: at its spawn, at each resume, and whenever it sends more to a Subagent still working. The delegating Agent is whichever Agent sent it — the Subagent's parent or another Subagent — and a Delegation names it wherever it stands. A Delegation stands in the Subagent's Transcript as a Message from the delegating Agent, drawn apart from a user Message, and it stands there only once the Subagent has received it: one never delivered — refused, or overtaken by the Subagent being stopped — stands nowhere. What a Delegation does is decided by how it is delivered, not by what the Subagent was doing when it was sent. One that begins a Turn is a spawn or a resume, as a Prompt begins a Turn in the Session a user addresses — even one sent while the Subagent worked, if that work finished before it arrived. One delivered into a Turn still working **steers** it, without beginning another Turn or adding a row to the parent's Transcript, and it stands at the point in that Turn where the Subagent received it. A Subagent's Session is offered Delegations in place of Prompts.
_Avoid_: Spawn prompt, task prompt, send message, instruction, queued message

**Errand**:
A single Provider call Suru makes for its own purposes rather than the user's: one Prompt in, one reply shaped by the schema the Errand asks for, carrying no Tools. An Errand belongs to no Session and appears in no Transcript, and it is never a Turn, because nothing about it is the user's work. A Provider runs an Errand however its own harness allows — without a Session where one-shot work is offered, and otherwise through a Session it starts and discards — and Suru stores nothing of it either way but the answer it asked for. An Errand that fails, times out, or answers outside its schema leaves no mark beyond the Log, because whatever asked for one always has something to fall back on.
_Avoid_: Background turn, side call, utility prompt

**Errand Selection**:
The Agent Selection a Provider declares for running Errands, chosen for cheapness and speed rather than capability, and resolved against that Provider's live Models each time an Errand runs, so a Model that has gone gives way to the Provider's default rather than failing the Errand. Whatever asks for an Errand decides which Provider runs it, and the Provider's own declaration then decides which Model: deriving a Title follows the Session's own Provider, and a Session that has selected no Provider runs no Errand at all. A Setting may pin one Agent Selection for every Session instead, with only explicitly chosen Model Options pinned and omitted ones following the selected Model's current defaults, which stands in front of the Provider's declaration rather than beside it — so the pinned Model is the one an Errand runs at, a Session that has selected no Provider is titled like any other, and the same resolution applies to the pin, a Model that has gone giving way to that Provider's default.
_Avoid_: Small model, cheap model, title model

**Message**:
User-visible content in a session attributed to the user or agent — or, for a Delegation, to the Agent that delegated it.
_Avoid_: Event, item

**Activity**:
User-visible progress, operational detail, or failure associated with a turn but not authored by the user or agent.
_Avoid_: Message, notification

**Tool**:
A capability a Provider gives an Agent that performs work on its behalf: the Provider's own, or Suru's, offered through the Broker. User-visible Tool execution is represented as Activity.
_Avoid_: Function, action

**Broker**:
The Tools Suru itself offers an Agent, served into every Provider Session beside the Provider's own, and which Tools those are depends on who is asking. Every Agent is offered the ones through which it reaches any Provider Suru hosts rather than only its own: to learn which Providers may be chosen, with their Availability, Models, and Model Options, and to spawn a brokered Subagent on one, read it, send it more, wait on it, and stop it. A Sidekick is offered more besides — the Tools that work across Suru itself — and its Subagents are not. Every Agent in a Session tree is offered the Broker, a Subagent as much as the Session the user addresses, within the depth and concurrency Settings pinned for it — the concurrency counted over the brokered Subagents working anywhere beneath the top-level Session, a spawn past it refused rather than queued — and a Broker Tool never asks an Approval, as a Provider's own spawning of a Subagent never does. A Provider's own way of spawning Subagents stays as it is; the Broker is offered beside it, never in its place.
_Avoid_: MCP server, tool server, bridge, plugin

**Sidekick**:
The Agent of a Session in the Sidekick Workspace, which the Broker offers the Tools to work across Suru itself rather than within one body of work: to learn which Workspaces and Sessions there are, read a Session, begin one, send one a Prompt, interrupt it, set it aside or bring it back, answer its Questionnaire, read and change Settings, and keep Memories. What it may not do is as much a part of it: it never deletes a Session, never decides an Approval, never changes an Approval Posture, never touches the Settings that govern Serving or a Pairing, and never adds, logs in to, or removes a Relay or changes whether its Server Serves through one, so nothing it does can widen what another Agent is allowed or open the machine to another. A Sidekick is a role rather than one conversation: each Session in the Sidekick Workspace has a Sidekick of its own, listed and settled like any other Session, and Memories are what one carries over to the next. The Tools are the Sidekick's alone; a Subagent it spawns is offered none of them. A Sidekick's reach is its own Server's: it works on a Remote's Sessions and Workspaces as its user's Client would, through its own Server and the Pairing, and never speaks to a Remote itself. Everything it does to a Session or a Workspace it may do on a Remote, each named with its Origin; Settings and Memories are its own Server's alone, since a Remote shares its work and not its administration. What it sent a Remote stands there as a Sidekick's on the Peer it came from, by that Peer's name, with nothing to follow back. An Unreachable Remote is named as not answering rather than answered for from what it last said, and an act on one of its Sessions is refused rather than kept for later; a Sidekick owed a Sidekick Report from a Remote that stops answering is told so once. A Workspace is named to a Sidekick with its presented root's path and its Description. No Sidekick acts on a Session of a Sidekick Workspace — its own, another's, or a Remote's, which refuses for itself — though it may read them, so a Sidekick never sets another Sidekick to work.
_Avoid_: Assistant, orchestrator, supervisor, meta-agent, admin session

**Subsession**:
A Session a Sidekick began, which remembers the Sidekick's Session that began it. A Subsession is a top-level Session in every way that matters to its work — the user may prompt it, it Settles, and it is listed like any other — and it is shown beneath its Sidekick's Session wherever that Session's tree is drawn, as a Subagent's would be, without being one: its working does not keep the Sidekick's Session Working, interrupting or deleting that Session leaves it alone, and its Usage and Cost are never rolled up beneath it. A Subsession has Subagents of its own like any Session and never Subsessions, since no Sidekick works in a Sidekick Workspace. Beginning one stands in the Sidekick's Transcript as an Activity of its own kind, naming the Subsession and what it was first asked, and leading into it. A Setting lets a reader leave Subsessions out wherever Sessions are listed — the Sidebar, its search, and the Session picker — on whichever Server they live; they are then reached through their Sidekick's Session, whose row carries their Standing for them, the open Session's highlight while one of them is open, and keeps from settling on its own while one of them still works, though none of their Interventions presents itself there. One whose Sidekick's Session has been deleted is listed again, since nothing else would lead to it, and a Session a Sidekick only acted on is never left out. On a Remote, a Subsession begun from a Peer heads its own tree, its Sidekick being elsewhere.
_Avoid_: Charge, child session, spawned session, Subagent (for a Session a Sidekick began)

**Sidekick Report**:
The account Suru gives a Sidekick of a Session it set to work — one it began, sent a Prompt, or answered — when that Session's Turn settles or the Session comes to owe an Intervention. It is delivered as a Subagent Report is: as the Sidekick's own input, steering a Turn still working and waking an idle Sidekick into a Continuation — its Session brought back if the user had set it aside — standing in no Transcript, and lost if Suru stops before it is taken. A Sidekick is told only of work it had a hand in, never of everything on the Server, and is owed Reports of each piece of work it set going until that work has settled: a Prompt it sent — a begun Session's first among them — from its admission until a Turn takes it, and that Turn until it settles; an Answer it gave, once delivered to the Agent, and the Turn it went on in until that Turn settles; and, past either Turn's settling, the Subagents that Turn set working, until the whole branch it set going has settled. A Turn takes a Prompt when it begins for it or its Agent takes it as a steer, and a steer the working Turn could not take that the Session carries as a Turn of its own makes that Turn the Sidekick's. A Subagent's work belongs to whichever Turn most recently set it working, spawning or resuming it, so one another Turn has since resumed tells the Sidekick nothing more, and one a Turn of the Sidekick's resumed is its own. Each such Turn is reported once as it settles, and each Questionnaire or Approval that Turn, or a Subagent of its branch, comes to owe while it works is reported once. Nothing else in the Session is the Sidekick's: a Turn the user or anyone else begins there, before or after, tells it nothing — a Subsession included, which reports the Turns the Sidekick's own Prompts set going and not the user's later ones — and neither does a Prompt of its own no Turn took — withdrawn, refused, or recorded in a Turn that settled before taking it — nor an Answer that never reached the Agent, nor anything once the Sidekick's own Session is deleted. Reading, listing, interrupting or setting a Session aside sets nothing going. A Report names the top-level Session it is about, as the Sidekick's Sessions are listed, and the Subagent's Session where what it tells of happened in one — the Turn of a Subagent the Sidekick answered, or an Intervention a Subagent owes.
_Avoid_: Session notification, callback, subscription, wake-up message

**Sidekick Workspace**:
The one Workspace on a Server that Suru itself owns: a directory outside source control, kept with that Server's own data and so separate for each Channel, and made the first time a Sidekick is asked for. It is a Workspace like any other — listed, searched, and chosen the same way, and the user may keep files of their own there — and being a Session of it is the whole of what makes that Session's Agent a Sidekick, however the Session was begun.
_Avoid_: Sidekick directory, home workspace, system workspace, managed workspace

**Memory**:
One thing a Sidekick chose to keep past its own Session, held by Suru rather than by any Provider, so a Sidekick on one Provider recalls what a Sidekick on another stored. A Memory is a short title, a body, and whatever tags its Sidekick gave it, with the moments it was stored and last changed; a Sidekick searches Memories by their words, tags, and dates, recalls one whole, changes it, or forgets it. Memories belong to the Server, not to a Session: deleting the Session that stored one leaves it standing, and only Sidekicks reach them. A Sidekick begins knowing the titles of the Memories most recently changed and nothing of what they say, so it knows what there is to recall without carrying it.
_Avoid_: Note, fact, knowledge, context (for a stored Memory); memory (for what an Agent holds in its context, which a Compaction changes)

**Questionnaire**:
A Provider-native request for structured user input during a Turn, containing one or more Questions. Distinct from a permission approval or a question written in an Agent's ordinary prose.
_Avoid_: Question tool, input request

**Question**:
One item in a Questionnaire asking the user for input.
_Avoid_: Prompt, field

**Answer**:
The submitted response to a Questionnaire, distinct from a Prompt: the user's own, or a Sidekick's given on the user's behalf, which says so as a typed property naming the Sidekick's Session and is drawn apart as a Sidekick's Prompt is.
_Avoid_: Reply, response message

**Approval**:
A Provider-native request for the user's consent before an Agent's Tool may act during a Turn, with a subject naming what is asked — a Command, a File Change, a read, network access, a permission grant, or another Tool by name. Distinct from a Questionnaire, which asks for input rather than consent, and from a question written in an Agent's ordinary prose. An Approval is answered with a Decision, and an Approval the Provider settles itself under its own rules never reaches the user.
_Avoid_: Permission prompt, confirmation, consent request

**Decision**:
The user's response to an Approval: Accept, Accept for Session, Decline, or Decline and Interrupt. Accept for Session lets the same request pass unasked for the rest of the Session; Decline and Interrupt refuses and ends the Turn.
_Avoid_: Answer, verdict, response

**Approval Posture**:
The Provider-native permission configuration under which a Session's Agent acts: for each Provider, the native values that Provider offers for deciding which Tool uses need an Approval. A Session follows its Provider's Setting until the user overrides it, and an override is then the Session's own, surviving resume and restart until reset. A change takes effect at once where the Provider allows it and at the next Turn where it does not, and never decides an Approval already pending. A native Subagent acts under the very posture of the Session that spawned it, and so does a brokered one on the same Provider; a brokered Subagent on another Provider acts under the rough equivalent of its spawner's posture, read from a fixed table between the Providers' values that errs toward asking where no twin exists, re-read whenever the spawner's changes and never set on the child itself.
_Avoid_: Permission mode, approval mode, trust level

**Intervention**:
An Approval awaiting a Decision or a Questionnaire awaiting an Answer: the one kind of thing a Session owes to its reader rather than to its Agent. An open Session's own Interventions present themselves, oldest first in Transcript order, whenever nothing else owns the keys and no panel is already open; a Subagent's or another Session's Interventions only mark the listing, never move the reader. A presentation never replaces work already in progress, opens with nothing chosen, and takes no key for a moment after appearing, so a reader mid-keystroke cannot decide by accident; the reader asking for one by key or click needs no such guard. Esc dismisses every Intervention pending at that moment, returning the composer until a new one arrives or the reader asks again, and the dismissal is the Client's own, kept only while it runs.
_Avoid_: Pending request, prompt, interrupt, notification

**Subagent**:
An agent to which a Turn's Agent delegates work, running its own conversation in its own Session — a child of the Session whose Turn spawned it. A Subagent is **native** when the delegating Agent's Provider spawned it, on that same Provider, and **brokered** when Suru spawned it through the Broker, on whichever Provider the delegating Agent chose; the two are told apart only where the route matters, and all that is said here holds for both. Where its Provider allows — and for a brokered Subagent, wherever its own Session can continue — a settled Subagent may be **resumed**: its parent's Agent, or another Subagent's, delegates more work to the same agent, continuing the same conversation, and that work is a new Turn in the Subagent's own Session rather than a new Subagent, so the Session holds the agent's whole conversation. The delegating Agent's Transcript — the parent's for the spawn, and for a resume whichever Agent delegated it, a sibling Subagent's included — records each stretch of delegated work as an Activity of its own kind, in the Turn that delegated it — or, for a stretch that begins only after that Turn has Settled, in whatever Turn of that delegating Agent's Session is active then or a Continuation it begins, even in a settled Subagent's Session: a row naming the Subagent and what it was asked to do, wearing the usual Marker while that stretch works and its outcome and duration once it settles, and standing — live or settled — as the way into the Subagent's Session, so every row of a resumed Subagent leads into the one Session. A resume finds its Subagent even across a Server stop wherever its Provider names the agent it resumes; one it cannot place is recorded as a new Subagent rather than lost. The Subagent itself never Settles: its Turns and its rows do, and between them it is simply not Working, as any Session may be, so a settled Subagent is one whose latest Turn has Settled. Those rows are all any other Transcript carries of it: the Subagent's work belongs to its own Transcript, never interleaved into the parent's. A Subagent may outlive the Turn that spawned it; while any Subagent still works the Session is still Working, and output one provokes after its Turn Settled lands in whatever Turn is active or begins a Continuation. Subagents may spawn Subagents, each recorded the same way one level down. Interrupting a Session whose Subagents still work stops them along with whatever else the Session is doing, and a single native Subagent may be stopped on its own where its Provider allows it, a brokered one always; neither asks before acting, as interrupting never does. A brokered Subagent keeps working when its parent's Provider process ends, and only interrupting or deleting the parent stops it.
_Avoid_: Task, child agent, background agent, worker; reopen, wake, revive (for a resume)

**Subagent Report**:
The account Suru gives the delegating Agent when a brokered Subagent's Turn settles — its outcome, what it failed with where it failed and the Subagent's Transcript says why, and a bounded excerpt of the Subagent's final Message, the rest readable through the Broker — delivered as that Agent's own input: it wakes an idle Agent into a Continuation and steers a Turn still working, a settled native Subagent waking into a Continuation of its own Session as a Watch would wake it. A brokered Subagent the user stops on its own is reported as stopped, since its parent planned on the result; one stopped by interrupting its parent reports nothing. A Report whose Agent has no Provider process to take it waits for the head of that Session's next Turn, and one delivered to a Session the user had settled makes it active again, as its Agent is working. A Report stands nowhere in any Transcript: the Subagent's row, settled, is the whole record, and the Subagent's own Transcript holds its words. A native Subagent's settling reaches its parent in the Provider's own way and is never a Subagent Report.
_Avoid_: Task notification, completion notification, callback, wake-up message

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
The Activity recording one shell command a Provider ran while working a Turn; a Tool that runs no shell command is recorded otherwise, never as a Command. Its command text is the command as a reader should see it: each Provider strips its own launcher plumbing — such as the shell wrapper it launches scripts through — before the Activity is recorded, so the stored text is the command itself, never the machinery around it. A command that arrives in a shape the Provider doesn't recognize as its own plumbing is recorded verbatim. A Command may also carry the **directory** it ran in. A Provider that reports none of its own has a leading change into an absolute directory, joined to the rest by `&&`, `;` or a new line, lifted out of the command text into that directory, so `cd /repo && cargo test` and `cd /repo; cargo test` are both recorded as `cargo test` run in `/repo`; should the change fail, the Command's output says so. Any other change of directory stays in the command text, since Suru cannot know where it leads, and so does the whole command when part of it may run elsewhere even though the change succeeded, as work sent to the background does. An Approval of a command is stricter still: it is decided before anything runs, so it names a directory only when every part of the command runs there or not at all, which lifts a change only when joined by `&&`.
_Avoid_: Shell invocation, exec

**File Change**:
The Activity recording one Provider-reported operation over one or more files in the Workspace, whichever Tool the Provider performed it with.
_Avoid_: Patch, file edit

**Tool Call**:
The Activity recording one use of a Tool that no more specific Activity records: a shell run is a Command, an edit a File Change, a delegation a Subagent row, a request for input a Questionnaire, and a Tool Call is everything else an Agent used a Tool for. A use whose effect another Activity already records is never also a Tool Call, and neither is a Provider's plumbing that only shapes its own interface. A Transcript shows Tool Calls unless a reader asks to hide them, which is a Setting; they are stored either way.
_Avoid_: Tool use, tool execution, function call

**Transcript**:
The ordered, user-visible history of a Session: its Messages and Activities in presentation order. A Prompt admitted to begin a Turn but not yet delivered is drawn by every client in the Transcript's position as the user Message it will become, so no reader waits on the Agent to see what was asked.
_Avoid_: History, log, conversation

**Working Indicator**:
The transient presentation immediately after a Session's latest Transcript row while that Session is Working or Monitoring. It distinguishes the Agent's own work in the current Turn from waiting on Subagents — those surviving a Turn that has Settled, or those the Agent waits on through the Broker while nothing else of its Turn is in progress — and from Monitoring. It carries the work's elapsed time and interruption guidance without becoming a Message or Activity; waiting on Subagents is still Working, so moving between the two changes only the words, never the time or the guidance. While Monitoring it names the Watch it waits on, or counts them where there are several, and only the word Monitoring shimmers.
_Avoid_: Active text, status text, loading row

**Session Content Column**:
The main working column of an open Session: its Transcript and Working Indicator, queued Prompts, latest-position affordance, composer extensions, composer, and composer footer. The Session header and the Landing sit outside it.
_Avoid_: Conversation column, transcript column

**Sidebar**:
The collapsible column a client shows beside its main view, listing Sessions with the active apart from the settled, searched by Title, and scoped to one Workspace or all of them. Whether it begins shown, how wide it begins, and how wide its scope begins, are Settings; showing or hiding it afterwards, and dragging its **edge** — the rule between it and the main view, painted in the Theme's focus color for as long as it is held — are the reader's own view state, kept only while the client runs. The one act that shows and hides it also brings the reader into it: asked for while it is hidden it shows and takes the keys, while it is shown without them it takes them, and only while it holds them does it hide, so reaching a Sidebar already on screen never costs the reader its place. The Sidebar is never narrower than its own floor and never leaves the main view less than its floor, so a width the reader asks for is drawn as wide as the terminal allows and restored whole when the terminal grows; a terminal too narrow for both it and the main view keeps the main view. An active row carries its Session's Standing twice over, from one reading: as the color of the Rail down its left, and as the word in its **right slot**. Its first line names the Workspace, preceded by its Remote when present, whatever the scope. Its third line presents the Worktree's current Checkout State, distinguishes a linked Worktree while leaving the main Worktree implicit, marks unavailable state explicitly, and stays blank for directories outside supported source control. While the Session is Working the slot reads **Working** with the duration the work has been running, and while it is Monitoring it reads **Monitoring** with the duration it has been waiting; a Standing with no word to say leaves the slot to the compact time since the Session last moved. The settled Sessions stand on a **shelf** below a **divider**: a rule the Sidebar draws only where something is settled, closing the active list and naming what the rows beneath it are. Shelf rows are slim where active rows are not, because settled work is history a reader keeps in view rather than work they are choosing between. The shelf is bounded: it opens on its first rows and the rest of the tail stands behind an affordance the reader asks for, batch by batch, until the shelf is whole. A **search box** at the top of the Sidebar narrows it by Title, and while it carries a query both shelves stand down: the Sidebar answers with one flat list of results, in the order the shelves would have drawn them and each keeping the shape its shelf gives it. Under it a **selector** says which Workspace the Sidebar is answering for and opens the ways into the reader's work: Everywhere first, then all of the Outlook's Workspaces, then every Workspace it has Sessions in and the one the client itself runs in, whether or not there is work there yet. Choosing a Workspace narrows both shelves and the results a query answers with; the Sidebar goes on listing the Outlook's whole body of work either way, because the entries are read off that listing and narrowing what it asks for would take the other Workspaces off the selector along with their Sessions. Choosing Everywhere widens the listing past the Outlook rather than narrowing it, and the selector offers no Workspace of another Server: a Workspace elsewhere is reached by turning the Outlook. Beside the selector an **add-Workspace affordance** opens a **path entry**, which stands in place of the list and takes what they type: a path they give relative is read from the current Execution Directory, and the owning Server resolves the directory it names into an Execution Directory and its Workspace. The directory becomes where the next Session works, while its Workspace becomes the client's current Workspace and what the selector narrows to; a bare Repository root selects only the Workspace and requires a working-copy choice before execution. A path standing at a file or at nothing is refused where the reader can see it and nothing moves. Where the open Session has a row in the list as presently drawn, that row alone carries the Sidebar's open-Session **highlight**: its Title drawn in the Theme's accent color, and nothing else about the row repainted, so the highlight never hides the Rail or the row's other readings; while a Subagent's Session is open the highlight stays on the row of the top-level Session whose tree it belongs to, since that tree is still what the main view is for; the Landing and an open Session narrowed or searched out of the list leave no row highlighted. The highlight says what the main view is for rather than whether its content has finished loading, where the keys are, or whether a Session is Working: opening a Session moves the highlight, clears the old content, and gives its composer the keys at once. Its header, Transcript, and other Session-derived content stand blank until they arrive; if that takes longer than 300 milliseconds, **Loading** appears in the Working Indicator's shimmer style until they do. The composer takes and keeps a draft under that Session from the first moment, but delivers no Prompt and offers no other Session-dependent act until the Session arrives. Failure leaves that Session open with its highlight and draft intact, replaces Loading immediately with a client-local, transcript-shaped **Error: Could not load Session:** row in the Theme's error color, and keeps delivery disabled; the error resembles Transcript content but never becomes part of the Transcript. Reaching the failed Session again through its Sidebar row retries it, clearing the error at once and beginning another quiet 300 milliseconds; without that row there is no retry, and the failed view stands until the reader goes elsewhere. A newer route — another Session, the Landing, or another Workspace — cancels an opening still under way and no late answer may pull the client back. While the reader drives the Sidebar, a separate Row Focus walks its controls and rows in their drawn order without changing the open Session. It is painted in the Theme's focus color only while the Sidebar owns the keys, beginning on the open Session where its row is drawn and on the Workspace selector otherwise; another surface taking the keys hides it until they return, and leaving the Sidebar gives it up. Up and Down wrap through every focusable control and readable Session row in their drawn order; a moving listing keeps focus on the same entry where it survives and otherwise carries it to the nearest one left. Row focus is painted whole, and a Rail is painted over its left column, so a row that is focused, open, and Working still says all three. Enter acts on what row focus stands over. The wheel answers to where the pointer stands rather than to who has the keys: over the Sidebar it moves the list beneath the search box and selector and never the Transcript, a whole row at a time and as far per step as the Transcript's wheel moves. Wheeling is looking rather than choosing, so it neither takes the keys nor moves row focus, and the list stays where the wheel left it — catching up included — until row focus moves or another Session opens and carries its row back into view. It stops at the end of the shelf as far as it has been walked rather than asking for more, and does nothing while a path entry or a row's context menu stands. A Session the client cannot read keeps a subdued row whose Title is followed by **[unreadable]**, a marker truncation never takes away; keyboard navigation passes it over, pressing it does nothing, and its context menu offers deletion but never settling. The other Session rows answer a pointer as readily as the keys: pressing one performs the same optimistic opening as Enter, without raising row focus where the Sidebar does not own the keys; a row asked for its **context menu** is offered the shelf it is not on — set aside, or brought back — along with deleting the Session, which asks again before it acts. A Sidebar on screen is as true as the server without the reader asking: every change the session-catalog stream reports — a Session made, retitled, deleted, set aside, brought back, its latest Turn begun or settled, or a whole catalog reconciled after a reconnection — is taken as it happens, and the Sidebar asks for its listing again to carry what the change itself does not say. Catching up is not the reader looking again, so it leaves the shelf as deep as they walked it and whatever they had opened standing.
_Avoid_: Panel, drawer, session list, nav; archive or section (for the settled shelf)

**Rail**:
The one-column stripe down the left of an active Sidebar row, spanning every line of the row, painted in the Theme's feedback color for the Session's Standing and absent where the Standing has nothing to say. Settled shelf rows carry none.
_Avoid_: Indicator bar, status stripe, gutter

**Standing**:
The one reading a listed Session presents about its work, from which both its Rail's color and its right slot's word derive: Needs Intervention, Working, Failed, Monitoring, Done, or nothing, in that order of precedence. Failed and Done hold only for a Settled latest Turn that no Client has Viewed since; an Interrupted Turn leaves nothing. Working and Monitoring map to the Theme's info color, Needs Intervention to warning, Failed to error, and Done to success. Needs Intervention marks a Session with an Intervention, whether its own or one belonging to a Subagent at any depth; the Answer or Decision belongs in the owning Session.
_Avoid_: Status (taken by Idle and Active), attention, state, condition

**Viewed**:
The Server's record of the last moment any Client had a Session open in its main view, reported by that Client when it opens the Session and again when a Turn Settles while it is open. It is one fact about the Session rather than about any one Client, so one Client Viewing a Session clears its Failed or Done Standing for every Client. A Session on screen counts as Viewed whether or not the reader is looking at the terminal.
_Avoid_: Read, seen, acknowledged

**Aside**:
The collapsible column a client shows on the far side of the main view from the Sidebar, answering for the open Session through a stack of Sections. It stands whenever the reader has it shown and a Session is open, whatever that Session has to say, so the Session Content Column never shifts because a Section found something to present; on the Landing there is nothing for it to answer for and it is absent, and it appears the moment a first Prompt is submitted, answering for the Provisional Session with that Session's lone entry until the real Session replaces it in place. Whether it begins shown and how wide it begins are Settings; showing or hiding it afterwards and dragging its edge are the reader's own view state, as they are for the Sidebar, and it keeps the same floor. The main view's floor outranks both columns, and where the terminal cannot keep all three the Aside gives way before the Sidebar, because the Sidebar is how the reader moves between their work and the Aside only reads more of it. The Aside can take the keys as the Sidebar does, walking its entries with a row focus of its own, and every act it offers a pointer it offers the keys too. It is shown, reached, and hidden by one act, just as the Sidebar is.
_Avoid_: Right sidebar, panel, inspector, drawer

**Section**:
One titled part of the Aside, presenting one reading of the open Session under a header naming it and counting what it holds. A Section with nothing to present says so where it stands rather than leaving the Aside, and one with more than its space can show scrolls rather than dropping any of it, keeping the open Session's entry in view.
_Avoid_: Widget, card, tab, pane

**Subagents Section**:
The Section listing the tree of Sessions the open Session belongs to: its top-level Session first, then every Subagent's Session beneath the one that spawned it, whether working or settled — one entry per Subagent however often it is resumed, standing beneath the Session that first spawned it. Among the Subagents one Session spawned, a branch with a working Subagent anywhere in it stands ahead of the settled ones, and within each the most recently spawned comes first; so the hierarchy is never broken to bring work forward, and a branch moves down when the last work in it settles and back up when any of it works again. Its header counts the Subagents at every depth and, while any of them work, how many beside it — `Subagents 6 (3 active)` — the working count in the working Marker's colour. The tree is the same wherever in it the reader stands, so the Section works as a switcher: the open Session's entry is highlighted, and choosing another entry opens that Session. A Subagent's entry is two lines, pressed, focused, and scrolled as one. Its first line wears the Marker of its Session's latest Turn that is a stretch of its work — passing over a Continuation begun only to hold another Subagent's row — Working while that Turn works, and otherwise the outcome it settled with — before its Title, which has the rest of the line. Its second line, indented where the Marker stood, carries its name dimmed, followed by ` · ` and the Model the Provider confirmed for it, where one is known, dimmed like the name — never the Model its parent's Agent Selection names — and, where the Marker's glyph alone would not tell, the word **Failed** or **Stopped** after them, so the Model stands between the name and the outcome word — and, right-aligned, how long it has worked, summed over its Turns — counting while it works and fixed once it settles — unless it waits on an Intervention, which it then says in place of the time — or, for a settled Subagent whose Session is Monitoring, says **monitoring** there instead. Where the line runs short, the name is left out so the Model has the room, and only then does the Model — or, where no Model is known, the name — give way to that slot, cut short with an ellipsis — never the slot to either. The tree's guides run on through the second line: the entry's own branch carries on to the sibling after it, and where the entry spawned Subagents of its own, the rule they hang from begins beneath its Marker. The top-level Session's entry is one line, carrying its Title, and its Working Marker and elapsed time while it is Working or Monitoring. An entry says it waits on an Intervention only where the Intervention is its own Session's, never on behalf of those beneath it — the tree already shows the whole family, and the Sidebar's Standing rolls it up. A Subagent that settled without Suru learning when its work ended leaves the time blank, its Marker saying enough. Moving between Sessions of one tree leaves the Section as it stands and moves only the highlight; a tree not yet in hand is drawn as the Sidebar draws a Session still opening, blank and then Loading, and one that cannot be read is answered by an error line in the Section until a Session in that tree is next opened. A tree whose Server has become Unreachable keeps what it last showed. Choosing an entry opens it and does nothing else; stopping a Subagent is left to the Subagent Picker and the Transcript. Where a Sidekick's Session heads the tree the Section answers for everything that Sidekick has a hand in, and its header says **Sessions** in place of Subagents, counting the same way. Beneath the Sidekick's own Subagents stand its Subsessions and every other Session it has acted on — sent a Prompt, answered, interrupted, set aside, or brought back; reading one is not acting on it — in one order, those working ahead of those not, and within each the one the Sidekick most recently acted on first, with nothing to tell a Subsession from a Session it only acted on. Each stands for as long as it and the Sidekick's Session both exist, settled or not, across a Server stop, and the reader cannot dismiss one. Such an entry is three lines, pressed, focused, and scrolled as one: its Marker and Title; then, dimmed, its Workspace with its Icon, preceded by its Remote's name where its Origin is one, the Remote's name giving way first where the line runs short; then the Model of its Agent Selection, dimmed, with **Failed** or **Stopped** where the Marker would not tell, and the right-aligned slot a Subagent's entry carries. It names no Checkout State. Each such Session's own Subagents stand beneath it in their usual two lines. A Subsession's tree is its Sidekick's, so opening one leaves the Section as it stands; a Session a Sidekick only acted on heads its own tree, and opening it shows that tree with no Sidekick above it, since several may have acted on it.
_Avoid_: Agents panel, subagent list, roster

**Subagent Picker**:
The docked list a client opens over the composer to browse the open Session's working Subagents, drawn as the tree they spawned in. It opens only while there is something to browse, so the key that opens it stays inert otherwise; moving through it and choosing an entry opens that Subagent's Session, a working entry may be stopped from its row, and closing it lands back where it opened. Settled Subagents are not its concern — they are reached from their rows in the Transcript. In a Sidekick's Session it lists the working Subsessions and other Sessions the Sidekick has acted on beside its Subagents, and stopping one of those interrupts it.
_Avoid_: Agent panel, roster, subagent list

**Workspace Picker**:
The centered, searchable list a Client opens to switch among the Workspaces its Sessions belong to and its current Workspace. Choosing one opens the Landing with that Client's last Execution Directory for the Workspace on its Server, defaulting to the main Worktree when none is remembered or requiring a Worktree choice for a bare Repository; the open Session keeps its own work and the Sidebar keeps its chosen scope. Beneath its rows it shows the Description of the Workspace the reader is on, in lines it keeps whether or not that Workspace has one, and a row offers editing that Description, which is set at the Workspace's Origin; saving it blank clears it, so it may be derived again.
_Avoid_: Project picker, project list, workspace switcher

**Icon Picker**:
The centered, searchable grid a Client opens to choose an Icon for one Session or one Workspace, reached from that Session's or Workspace's row or from the open Session's header, always replacing whatever Icon stood there and never clearing it. Its search narrows the Icon Catalog by name and keyword, its cells show glyphs alone with the focused glyph named beneath the grid, and it stays inert, its ways in withheld, while Icons are not drawn — a glyph one cannot see is not one to choose.
_Avoid_: Emoji picker, glyph chooser, icon menu

**Worktree Selector**:
The Landing control that chooses an existing Worktree for the next Session or asks Suru to create a new one on first Prompt submission. It names the current location above its list without offering it as a choice, then offers creating a new Worktree first and selected, then every Worktree of the Repository with its Checkout State and its location: the leaf name alone for a Worktree Suru manages, the path beneath the Workspace's main root for one within it, and the whole path otherwise. The Worktree the reader is already in is marked among them, and choosing it keeps the location as it stands. Choosing another existing Worktree or creating a new one starts at its root; a subdirectory is reached through the Sidebar's path entry instead.
_Avoid_: Workspace Picker, branch picker

**Truncation**:
The condition of a Message, command Activity, Tool Call, Reasoning Activity, or Compaction's summary whose stored content Suru's cap cut short of everything the Provider sent. Truncation is carried as a typed property beside the content rather than as text within it, so a client reads it as data and draws its own **truncation marker**: the line a Transcript shows in place of what the cap dropped.
_Avoid_: Elision, clipping

**Fold**:
The compact presentation a client's Transcript gives an entry whose full stored content remains available. A Fold is reversible, client-local view state: expanding it fully reveals everything stored, and folding never alters stored content. A folded entry shows a **fold marker**: the line indicating how much the Fold hides. A Fold may open in stages: a **Peek** is an intermediate step that reveals part of what the Fold hides — such as the tail of a command Activity's output — while the fold marker counts what remains hidden. Distinct from Truncation, which is a condition of the stored content itself; a single entry can carry both.
_Avoid_: Collapse, elision, hide; preview (for Peek)

**Group**:
The reversible, client-local presentation a Transcript gives a run of one or more adjacent Activities of the same groupable kind — commands, Tool Calls, or Reasoning — without changing stored content or presentation order. An Activity belongs to its Group from the moment it starts and stays in it however it settles, so a Group stands in one place for its whole run and a lone command reads _Ran 1 command_. A Group's marker counts every member, names the work still running while any member runs, and says how many of its members failed; Tool Calls and commands never share a Group. Opening a Group reveals its members in their kind's own Fold presentation. Whether a Transcript opens its Groups collapsed or expanded, or forms none at all, is a Setting.
_Avoid_: Batch, merge, cell

**Turn Fold**:
The single-marker presentation a client's Transcript gives a settled Turn, hiding the work between the Turn's opening user Message and its outcome. Like a Fold, a Turn Fold is reversible, client-local view state, but it keys on the Turn's Settle rather than on entry adjacency, and it opens in one step rather than in stages. Its marker names how the Turn settled — worked, stopped, or failed — with the Turn's duration when known. A Turn's user Messages and Delegations, its final agent Message, its Compactions, and a failed Turn's terminal Error Activity stay visible outside it; entries revealed by expanding keep their own Fold and Group state. The marker stands in the same place folded or expanded, because it is the row a reader clicks to move between the two: folded it stands for the work, and expanded it heads the work it opened onto. A Turn Fold also manages itself around the reader's attention, which a Fold does not: interrupting a Turn opens the fold it is about to settle into so the reader keeps their place, and a newer Turn beginning folds back the Turns before it that the reader had opened — the interrupted one included — because compressing past work is what the fold is for.
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
The state in which a Server accepts connections from other machines' Servers, off unless its user turns it on as a Setting, and never extended to the machine's own Clients — they attach the way they always have. A Serving Server is reached by the ways its user turns on, each on its own: at addresses of its own, and through any Relay it holds a Login at and has chosen to Serve through. Holding a Login opens nothing by itself.
_Avoid_: Hosting, remote mode, exposing

**Invite**:
The one-time pasteable string a Serving Server issues so another machine's Server may form a Pairing with it: the ways of reaching it its user chose to offer — its own addresses, the Relays it Serves through, or both — the Server's identity, and a token spent by its first redemption, dead after a short while unused, and superseded by the next Invite either way.
_Avoid_: Connect string, join code, ticket

**Pairing**:
The durable relationship formed when one Server redeems another's Invite: each side holds the other's identity and trusts nothing else, so the two find each other again on their own until one side removes the other. A Pairing is one-way — the redeeming Server reaches into the Serving one, never the reverse — and a second Pairing in the opposite direction is its own relationship.
_Avoid_: Link, tunnel, connection (for the relationship itself)

**Remote**:
A paired Serving Server as the redeeming side knows it: carrying a name its user gave it — offered from the machine's own hostname, theirs to change, and anything but `everywhere`, which names every Server at once — reached only through the local Server, and offering its own Sessions and Workspaces for a Client to work in. What a Remote's user does on their own machine is none of the local side's business; a Remote shares its work, not its administration. A Remote is an entry its user can list and remove: removing it ends the Pairing on this side for certain and, when the Remote answers within a moment, on its side too — one that does not answer is forgotten here all the same, and reaching it again takes a new Invite.
_Avoid_: Remote server, host, upstream; forget (removal is the one verb for ending a Pairing from either side)

**Peer**:
A paired redeeming Server as the Serving side knows it: an entry its user can list and remove, and removing it ends the Pairing. A Peer may also withdraw — its own user removing the Remote on their side — and a withdrawn Peer simply leaves the list; only the Serving user's own removal marks it revoked.
_Avoid_: Authorized client, key entry

**Unreachable**:
The state of a paired Remote that has stopped answering by every way it offers while its Pairing stands, which the local Server keeps trying to reach on its own until it answers again. Distinct from a Remote whose Pairing has ended. A Remote out of reach because a Relay needs a login is Unreachable like any other, and its offer to try again says a login is needed. A Relay that has stopped answering reads Unreachable in the same sense — its Login stands and the Server keeps trying on its own — which is distinct from one that reads **login needed**, where trying cannot help until the user logs in. Wherever Sessions are listed, an Unreachable Remote keeps the rows it last gave, dimmed, and stands as a slim subdued row of its own at the foot of the active list marked **[unreachable]** — a row that offers to try again and that leaves as soon as the Remote answers. A Session whose Origin is Unreachable may still be opened and read, but nothing done to it goes anywhere until the Remote answers: a Prompt is refused where the reader can see it and kept in the composer, any command or picker that would ask that Remote is refused the same way, and any Intervention of that Session waits out of sight. Everything else the Client offers that does not touch that Remote — the Sidebar, other Sessions, Settings, turning the Outlook, quitting — stays as live as it ever was.
_Avoid_: Offline, disconnected, down, reconnecting (the Client's word for the Remote is the state, not the local Server's effort)

**Relay**:
A server through which Servers that cannot reach each other directly carry a Pairing: each connects outward to it, and it passes what they say between them without being able to read it or to speak as either. A Relay admits by login and joins only Servers whose Logins stand under one Account, and it is never what makes one Server trust another — that remains the Invite. A Relay is an entry a Server's user can list and remove, and a Server may hold several.
_Avoid_: Relay server, proxy, broker, hub, gateway, tunnel

**Account**:
The user as one Relay knows them: what an identity at an identity provider logs in as, and what a Login stands under. Two identities are never taken for one Account on the strength of a shared name or address, and an Account at one Relay is nothing to another.
_Avoid_: User, tenant, identity (an identity is the provider's; an Account is the Relay's)

**Login**:
A Server's lasting standing at a Relay under one Account, formed once through an identity provider and tied to that Server's identity, so each Server on a machine holds its own. A Login does not lapse with time: it stands until its Server's user removes the Relay or the Relay's operator removes the Login. While its Account no longer satisfies the Relay — the Relay's own finding, never the Server's — the Relay refuses the Login without forgetting it, the Relay's entry reads **login needed**, and one fresh login from any Server of that Account restores them all.
_Avoid_: Session, registration, enrollment (a Pairing's word)

**Outlook**:
The Server whose world a Client is presently looking into: its own machine's Server unless the user has turned it toward a Remote. A Client holds one Outlook at a time, and everything it presents that acts or begins — the Landing, the Skill Catalog, the pickers of Agents and Workspaces, the Session it offers to begin — answers for that Server alone. A listing of Sessions is the one thing that may range wider, when its scope is Everywhere; opening a Session there turns the Outlook toward that Session's Origin, and the Outlook's current Workspace becomes the one the Client remembered for it or, where it remembered none, the opened Session's own. The user may also turn it deliberately, which resolves the Remote's own working directory instead. A Client always shows which Remote its Outlook is turned toward, so a turn is never silent. A Remote whose Pairing ends while the Outlook is turned toward it turns the Outlook back to the Client's own Server, with an error where the reader can see it. A Remote that merely becomes Unreachable does not turn it: the Outlook stays where the user left it, and what it offers to begin waits with the Remote, so a user who wants to begin work elsewhere turns it themselves.
_Avoid_: Scope (the Sidebar's Workspace narrowing already owns it), view, context, focus

**Origin**:
The Server a Session lives on: the Client's own machine's Server, or a Remote known by its name. A Session's identity is only unique within its Origin, so every Session a Client can reach is held as identity and Origin together, and everything done to a Session — opening, prompting, settling, deleting — goes to its Origin however the Client came by the row. Wherever Sessions from more than one Server stand together, a row whose Origin is a Remote carries that Remote's name, and an open Session's header carries it likewise; a Session of the Client's own Server carries nothing, because the ordinary case stays quiet.
_Avoid_: Source, host, server tag, home

**Everywhere**:
The scope under which a listing of Sessions ranges over every Server the Client can reach — its own and every paired Remote, whether or not that Remote currently answers — across all their Workspaces. It stands first in the Sidebar's selector, is one of the states the Session picker cycles through, and may be named by the Setting that gives the Sidebar its starting scope. A Remote that has paired into this machine as a Peer is not reached by it, because a Pairing is one-way; a machine the reader wants to see is one they pair with from their side. Everywhere lists in the one flat order the shelves always draw, by recency as each Server reports it, without grouping by Server and without correcting one machine's clock against another's. While it is chosen every Remote is kept in view, whether or not the surface listing it is on screen, so the listing is as true as its Servers the moment it is shown. An Unreachable Remote keeps its place in the listing as the Unreachable entry describes; one whose Pairing has ended takes its rows with it. Everywhere changes what is listed and nothing about where work begins: choosing it leaves the Outlook where it stands, and opening a row turns the Outlook toward that row's Origin with the scope still chosen.
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

**Row Focus**:
The transient mark a list the reader drives from the keys keeps on the one entry they stand over, which Enter acts on. As the keys move it, the list scrolls only as far as keeps two entries beyond it in view, ahead and behind, and not at all while the list's end in that direction already shows, so the reader walks toward the edge of what is shown before the list moves under them. Those two are entries focus could stand on, however many Rows each takes, with whatever headers or dividers stand between them; a list too short for two either side keeps as many as it can spare evenly. A list following the open Session's entry rather than the keys keeps the same margin around it. A list opens scrolled as though the keys had walked there from its head, holds where it stands when its entries change or the pointer moves focus, gives way when it grows shorter only as far as keeps in view the focus it was showing, and answers the keys again from wherever it was left.
_Avoid_: Highlight, cursor, selection (for the focused entry)

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
