//! Reading a Session for an Agent: its Transcript rendered as compact text an
//! Agent reads, rather than rows a screen draws. It reads the same stored
//! Messages and Activities the TUI's projection draws from, and it is what a
//! Sidekick's `read_session` answers with.
//!
//! The projection owns everything about what a read holds: the window of
//! Turns, the cap on characters counted back from the window's end, the
//! detail it is read at, the point a later read continues from, and the
//! statement of everything left out, so a cut never goes unstated. Reasoning
//! is never rendered, at any detail. It is a pure function of a Session's
//! snapshot, so a Session held by this Server and one fetched from a Remote
//! read alike.
//!
//! The cap counts what the Session's Messages and Activities say, never what
//! a reading says about them — the headings, the numbers and labels that
//! begin each entry's line, the marks of a cut — so any cap of a character
//! or more shows something of the newest entry a read reaches, and the read
//! after it moves on.
//!
//! Every Message and Activity a reading can show is numbered by where it
//! stands: its Turn, counted from the Session's first, and its place among
//! that Turn's entries — `4.7`. Turns and their entries are only ever
//! appended, so a number names the same entry for as long as the Session
//! lasts, and a point to read back from is a number and a count of characters
//! into that entry — `4.7.120` — which a later read finds where an earlier one
//! left it.

use std::{collections::HashMap, fmt, path::Path, str::FromStr};

use time::{OffsetDateTime, macros::format_description};

use crate::protocol::{
    Activity, ActivityStatus, Answer, Approval, ApprovalOutcome, ApprovalSubject, Author,
    CompactionTrigger, Decision, FileChange, Message, MessageRole, MessageStatus, Question,
    QuestionAnswer, Questionnaire, QuestionnaireOutcome, SessionSnapshot, SessionTimestamp,
    TranscriptItem, Turn, TurnStatus, WatchOutcomeStatus,
};

/// How many Turns a reading holds unless asked for another number.
pub(crate) const DEFAULT_TURNS: usize = 1;

/// How many characters of what its entries say a reading shows at most
/// unless asked for another number.
pub(crate) const DEFAULT_MAX_CHARS: usize = 2_000;

/// The most characters of an Activity's line, or of anything quoted in a
/// heading or a statement, before it is shortened with an ellipsis. The entry
/// read whole holds all of it.
const LINE_CHARS: usize = 160;

/// The most characters of a Sidekick's Title a line names it by before it is
/// shortened with an ellipsis, kept short since every line it sent says it.
const TITLE_CHARS: usize = 60;

/// What stands where a line's start was cut by the cap.
const CUT: &str = "[…]";

/// What follows anything Suru's own storage cap cut short.
const STORED_CUT: &str = " [cut short when stored]";

/// How much a reading shows of each Turn it holds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Detail {
    /// The user Messages and Delegations that asked for each Turn's work, and
    /// the Agent's final Message in it.
    #[default]
    Messages,
    /// Every Message, and one line for each Activity, without its output.
    Activities,
}

impl Detail {
    pub(crate) const NAMES: [&'static str; 2] = ["messages", "activities"];

    pub(crate) fn named(name: &str) -> Option<Self> {
        match name {
            "messages" => Some(Self::Messages),
            "activities" => Some(Self::Activities),
            _ => None,
        }
    }
}

/// One Message or Activity as a reading numbers it: its Turn, counted from
/// the Session's first, and its place among that Turn's entries, each from 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EntryNumber {
    turn: usize,
    entry: usize,
}

impl EntryNumber {
    /// The number of the Turn the entry stands in.
    pub(crate) const fn turn(self) -> usize {
        self.turn
    }
}

impl fmt::Display for EntryNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.turn, self.entry)
    }
}

impl FromStr for EntryNumber {
    type Err = ();

    fn from_str(spelled: &str) -> Result<Self, ()> {
        let (turn, entry) = spelled.trim().split_once('.').ok_or(())?;
        Ok(Self {
            turn: counted(turn)?,
            entry: counted(entry)?,
        })
    }
}

/// A point in a Transcript to read back from: before the given number of
/// characters of an entry — or before the entry itself, or before a whole
/// Turn. Spelled as briefly as says it: `4`, `4.7` or `4.7.120`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Position {
    turn: usize,
    entry: usize,
    chars: usize,
}

impl fmt::Display for Position {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.entry, self.chars) {
            (1, 0) => write!(formatter, "{}", self.turn),
            (entry, 0) => write!(formatter, "{}.{entry}", self.turn),
            (entry, chars) => write!(formatter, "{}.{entry}.{chars}", self.turn),
        }
    }
}

impl FromStr for Position {
    type Err = ();

    fn from_str(spelled: &str) -> Result<Self, ()> {
        let mut parts = spelled.trim().split('.');
        let turn = counted(parts.next().ok_or(())?)?;
        let entry = parts.next().map_or(Ok(1), counted)?;
        let chars = match parts.next() {
            None => 0,
            Some(chars) => chars.parse::<usize>().map_err(|_| ())?,
        };
        if parts.next().is_some() {
            return Err(());
        }
        Ok(Self { turn, entry, chars })
    }
}

/// A number counted from 1, as Turns and entries are.
fn counted(spelled: &str) -> Result<usize, ()> {
    match spelled.parse::<usize>() {
        Ok(0) | Err(_) => Err(()),
        Ok(number) => Ok(number),
    }
}

/// What a read asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadRequest {
    /// The latest Turns, or those before a point, capped from their end.
    Window(Window),
    /// One Message or Activity, whole.
    Entry(EntryNumber),
}

/// Which Turns a read holds, how much of them, and how much it shows of each.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Window {
    pub(crate) turns: usize,
    pub(crate) max_chars: usize,
    pub(crate) before: Option<Position>,
    pub(crate) detail: Detail,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            turns: DEFAULT_TURNS,
            max_chars: DEFAULT_MAX_CHARS,
            before: None,
            detail: Detail::Messages,
        }
    }
}

/// Why a read names nothing the Session holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadRefusal {
    /// A Turn the Session has not reached; it has `turns`.
    NoSuchTurn { turn: usize, turns: usize },
    /// An entry its Turn does not hold; that Turn holds `entries`.
    NoSuchEntry { entry: EntryNumber, entries: usize },
}

/// What a read of a Session finds, beside what its snapshot already says of
/// it.
#[derive(Debug)]
pub(crate) struct SessionReading<'a> {
    /// Every Questionnaire waiting on an Answer, in Transcript order.
    pub(crate) questionnaires: Vec<OpenQuestionnaire<'a>>,
    /// Each Approval waiting on the user's Decision, said as a sentence
    /// naming what it asks, in Transcript order.
    pub(crate) approvals: Vec<String>,
    /// Each Subagent's Session beneath this one that waits on the user, said
    /// as a sentence.
    pub(crate) subagent_interventions: Vec<String>,
    /// The Turns read, or the entry read whole.
    pub(crate) transcript: String,
    /// The point to read back from for whatever was left out before the
    /// transcript, and `None` when it reaches the Session's beginning.
    pub(crate) before: Option<Position>,
    /// What was left out before the transcript, said as a sentence naming
    /// `before`; `None` exactly when `before` is.
    pub(crate) earlier: Option<String>,
}

/// A Questionnaire waiting on an Answer, and its entry's number.
#[derive(Debug)]
pub(crate) struct OpenQuestionnaire<'a> {
    pub(crate) entry: EntryNumber,
    pub(crate) questionnaire: &'a Questionnaire,
}

/// Reads `session` as `request` asks.
pub(crate) fn read<'a>(
    session: &'a SessionSnapshot,
    request: &ReadRequest,
) -> Result<SessionReading<'a>, ReadRefusal> {
    let transcript = Transcript::of(session);
    let (text, start) = match request {
        ReadRequest::Window(window) => transcript.window(window)?,
        ReadRequest::Entry(number) => (transcript.whole(*number)?, None),
    };
    Ok(SessionReading {
        questionnaires: transcript.open_questionnaires(),
        approvals: transcript.awaited_decisions(),
        subagent_interventions: subagent_interventions(session),
        transcript: text,
        before: start.map(Bound::position),
        earlier: start.map(|start| start.statement()),
    })
}

/// One Message or Activity a reading can show. Reasoning is none: it is left
/// out before anything is numbered.
#[derive(Clone, Copy)]
enum Entry<'a> {
    Message(&'a Message),
    Activity(&'a Activity),
}

/// One Turn and what stands in it, in Transcript order.
struct TurnEntries<'a> {
    turn: &'a Turn,
    entries: Vec<Entry<'a>>,
    /// Where the Agent's last Message with anything in it stands.
    final_message: Option<usize>,
}

/// A Session's Turns and their entries, numbered as a reading numbers them.
struct Transcript<'a> {
    session: &'a SessionSnapshot,
    turns: Vec<TurnEntries<'a>>,
}

/// A point in the Transcript, each part counted from zero: before `chars`
/// characters of entry `entry` of Turn `turn`. The end of a Turn is the entry
/// past its last, and the end of the Transcript the Turn past its last.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Bound {
    turn: usize,
    entry: usize,
    chars: usize,
}

impl Bound {
    fn position(self) -> Position {
        Position {
            turn: self.turn + 1,
            entry: self.entry + 1,
            chars: self.chars,
        }
    }

    /// The sentence saying what stands before this point, and how to read it.
    fn statement(self) -> String {
        let turn = self.turn + 1;
        let mut parts = Vec::new();
        if self.chars > 0 {
            parts.push(format!(
                "the first {} of {turn}.{}",
                plural(self.chars, "character"),
                self.entry + 1
            ));
        }
        match self.entry {
            0 => {}
            1 => parts.push(format!("entry {turn}.1")),
            entries => parts.push(format!("entries {turn}.1–{turn}.{entries}")),
        }
        match self.turn {
            0 => {}
            1 => parts.push("Turn 1".to_owned()),
            turns => parts.push(format!("Turns 1–{turns}")),
        }
        let mut statement = format!(
            "Left out before this: {}. Call again with before \"{}\" to read on",
            joined(&parts),
            self.position()
        );
        if self.chars > 0 {
            statement.push_str(&format!(
                ", or with item \"{turn}.{}\" to read that entry whole",
                self.entry + 1
            ));
        }
        statement.push('.');
        statement
    }
}

impl<'a> Transcript<'a> {
    fn of(session: &'a SessionSnapshot) -> Self {
        let messages = session
            .messages
            .iter()
            .map(|message| (message.id, message))
            .collect::<HashMap<_, _>>();
        let activities = session
            .activities
            .iter()
            .map(|activity| (activity.id(), activity))
            .collect::<HashMap<_, _>>();
        let mut turns = session
            .turns
            .iter()
            .map(|turn| TurnEntries {
                turn,
                entries: Vec::new(),
                final_message: None,
            })
            .collect::<Vec<_>>();
        let turn_of = session
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| (turn.id, index))
            .collect::<HashMap<_, _>>();
        for item in &session.transcript {
            let (turn_id, entry) = match item {
                TranscriptItem::Message { message_id } => match messages.get(message_id) {
                    Some(message) => (message.turn_id, Entry::Message(message)),
                    None => continue,
                },
                TranscriptItem::Activity { activity_id } => match activities.get(activity_id) {
                    Some(Activity::Reasoning { .. }) | None => continue,
                    Some(activity) => (activity.turn_id(), Entry::Activity(activity)),
                },
            };
            if let Some(turn) = turn_of.get(&turn_id).map(|index| &mut turns[*index]) {
                if matches!(entry, Entry::Message(message)
                    if message.role == MessageRole::Agent && !message.content.is_empty())
                {
                    turn.final_message = Some(turn.entries.len());
                }
                turn.entries.push(entry);
            }
        }
        Self { session, turns }
    }

    /// The point `position` names, if this Transcript holds its Turn and
    /// entry. A point past the end of the entry it falls in stands at that
    /// end: its entry may say less than when the point was given, and reading
    /// all of it loses nothing.
    fn bound(&self, position: Position) -> Result<Bound, ReadRefusal> {
        let turn = self.turn(position.turn)?;
        let entries = turn.entries.len();
        if position.entry > entries + 1 {
            return Err(ReadRefusal::NoSuchEntry {
                entry: EntryNumber {
                    turn: position.turn,
                    entry: position.entry,
                },
                entries,
            });
        }
        let chars = match turn.entries.get(position.entry - 1) {
            Some(entry) if position.chars > 0 => {
                position.chars.min(self.content(*entry).chars().count())
            }
            _ => 0,
        };
        Ok(Bound {
            turn: position.turn - 1,
            entry: position.entry - 1,
            chars,
        })
    }

    fn turn(&self, turn: usize) -> Result<&TurnEntries<'a>, ReadRefusal> {
        turn.checked_sub(1)
            .and_then(|index| self.turns.get(index))
            .ok_or(ReadRefusal::NoSuchTurn {
                turn,
                turns: self.turns.len(),
            })
    }

    /// The Turns `window` asks for, showing at most its cap of what their
    /// entries say, and the point the text begins at where anything stands
    /// before it.
    ///
    /// Within the Turn a point falls in, the entry it falls within is shown
    /// whatever the detail: a read only ever gives a point within an entry it
    /// showed, or at the end of one it had no room left for, and that entry
    /// is read on from there though a later Message has since made it no
    /// longer its Turn's final one.
    fn window(&self, window: &Window) -> Result<(String, Option<Bound>), ReadRefusal> {
        let end = match window.before {
            Some(position) => self.bound(position)?,
            None => Bound {
                turn: self.turns.len(),
                entry: 0,
                chars: 0,
            },
        };
        // The Turns holding anything before the end, the Turn the end falls
        // in counting only where something of it does.
        let reached = if end.entry > 0 || end.chars > 0 {
            end.turn + 1
        } else {
            end.turn
        };
        let first = reached.saturating_sub(window.turns.max(1));
        // A cap of nothing would show nothing and move no read on, so the
        // least a read shows is one character.
        let mut budget = window.max_chars.max(1);
        let mut lines = Vec::new();
        let mut start = Bound {
            turn: first,
            entry: 0,
            chars: 0,
        };
        'turns: for turn in (first..reached).rev() {
            if budget == 0 {
                // A newer Turn spent the cap; this one is left for the read
                // after, rather than shown as a heading with nothing beneath.
                start = Bound {
                    turn: turn + 1,
                    entry: 0,
                    chars: 0,
                };
                break;
            }
            let (whole, partial) = if turn == end.turn {
                (end.entry, end.chars)
            } else {
                (self.turns[turn].entries.len(), 0)
            };
            let pointed = (partial > 0).then_some(whole);
            let heading = self.heading(
                turn,
                whole + usize::from(partial > 0),
                window.detail,
                pointed,
            );
            let shown = (0..whole)
                .map(|entry| (entry, None))
                .chain(pointed.map(|entry| (entry, Some(partial))));
            for (entry, available) in shown.rev() {
                let Some(line) = self.line(turn, entry, window.detail, available) else {
                    continue;
                };
                let said = line.content.chars().count();
                if said <= budget {
                    budget -= said;
                    lines.push(line.whole());
                    continue;
                }
                // The cap falls within this entry: it keeps what fits of its
                // end, or — where a newer entry spent the cap — nothing, and
                // the point to read on from falls at its end, so the next read
                // shows it whatever has been written since.
                start = Bound {
                    turn,
                    entry,
                    chars: said - budget,
                };
                if budget > 0 {
                    lines.push(line.tail(budget));
                }
                lines.push(heading);
                break 'turns;
            }
            lines.push(heading);
            start = Bound {
                turn,
                entry: 0,
                chars: 0,
            };
        }
        lines.reverse();
        let start = (start
            > Bound {
                turn: 0,
                entry: 0,
                chars: 0,
            })
        .then_some(start);
        Ok((lines.join("\n"), start))
    }

    /// The line that opens Turn `turn`, read up to its first `entries`
    /// entries — `pointed` among them shown whatever the detail: its number,
    /// whether it is a Continuation, how it stands, and what `detail` leaves
    /// out of those entries.
    fn heading(
        &self,
        turn: usize,
        entries: usize,
        detail: Detail,
        pointed: Option<usize>,
    ) -> String {
        let TurnEntries {
            turn: read,
            entries: all,
            ..
        } = &self.turns[turn];
        let mut heading = format!("[Turn {} of {}", turn + 1, self.turns.len());
        if self.is_continuation(turn) {
            heading.push_str(" · Continuation");
        }
        let at = |moment: Option<SessionTimestamp>| {
            moment.map_or_else(String::new, |moment| format!(" at {}", spelled(moment)))
        };
        let after = read
            .worked_ms()
            .map_or_else(String::new, |worked| format!(" after {}", duration(worked)));
        match read.status {
            TurnStatus::Active => {
                heading.push_str(" · working");
                if let Some(started) = read.started_at {
                    heading.push_str(&format!(" since {}", spelled(started)));
                }
            }
            TurnStatus::Completed => {
                heading.push_str(&format!(" · completed{}{after}", at(read.settled_at)));
            }
            TurnStatus::Interrupted => {
                heading.push_str(&format!(" · interrupted{}{after}", at(read.settled_at)));
            }
            TurnStatus::Failed => {
                heading.push_str(&format!(" · failed{}{after}", at(read.settled_at)));
                let failure = all.iter().rev().find_map(|entry| match entry {
                    Entry::Activity(Activity::Error { text, .. }) => Some(text),
                    _ => None,
                });
                if let Some(failure) = failure {
                    heading.push_str(&format!(": {}", one_line(failure)));
                }
            }
        }
        if detail == Detail::Messages {
            let (mut messages, mut activities) = (0, 0);
            for (index, entry) in all.iter().enumerate().take(entries) {
                if pointed == Some(index) || self.shows(turn, index, detail) {
                    continue;
                }
                match entry {
                    Entry::Message(message) if message.content.is_empty() => {}
                    Entry::Message(_) => messages += 1,
                    Entry::Activity(_) => activities += 1,
                }
            }
            let left_out = [
                (messages > 0).then(|| plural(messages, "agent Message")),
                (activities > 0).then(|| plural(activities, "Activity")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if !left_out.is_empty() {
                heading.push_str(&format!(" · {} not shown", joined(&left_out)));
            }
        }
        heading.push(']');
        heading
    }

    /// Whether Turn `turn` is a Continuation: one begun with nothing asked of
    /// the Agent. In a top-level Session every other Turn is begun by a
    /// Prompt. In a Subagent's Session its spawn begins the first, and a
    /// Delegation heads any other it begins; one its Provider gave Suru no
    /// words of leaves nothing to tell it from a Continuation, and reads as
    /// one.
    fn is_continuation(&self, turn: usize) -> bool {
        let TurnEntries {
            turn: read,
            entries,
            ..
        } = &self.turns[turn];
        if !read.is_continuation() {
            return false;
        }
        if !self.session.session.is_subagent() {
            return true;
        }
        let delegated = matches!(
            entries.first(),
            Some(Entry::Message(Message {
                role: MessageRole::Delegation(_),
                ..
            }))
        );
        turn > 0 && !delegated
    }

    /// Whether `detail` shows entry `entry` of Turn `turn`.
    fn shows(&self, turn: usize, entry: usize, detail: Detail) -> bool {
        let turn = &self.turns[turn];
        match turn.entries[entry] {
            Entry::Message(message) if message.content.is_empty() => false,
            Entry::Message(message) => match message.role {
                MessageRole::Agent => {
                    detail == Detail::Activities || turn.final_message == Some(entry)
                }
                MessageRole::User | MessageRole::Delegation(_) => true,
            },
            Entry::Activity(_) => detail == Detail::Activities,
        }
    }

    /// Entry `entry` of Turn `turn` as `detail` shows it, or of its first
    /// `available` characters where the read ends within it, which is shown
    /// whatever the detail; `None` where `detail` shows nothing of it.
    fn line(
        &self,
        turn: usize,
        entry: usize,
        detail: Detail,
        available: Option<usize>,
    ) -> Option<Line> {
        if available.is_none() && !self.shows(turn, entry, detail) {
            return None;
        }
        let read = self.turns[turn].entries[entry];
        let number = EntryNumber {
            turn: turn + 1,
            entry: entry + 1,
        };
        let content = self.content(read);
        let content = match available {
            Some(chars) => first_chars(&content, chars).to_owned(),
            None => content,
        };
        let suffix = match read {
            Entry::Message(message) if message.truncated => STORED_CUT,
            _ => "",
        };
        Some(Line {
            label: format!("{number} {}: ", self.label(read)),
            content,
            suffix,
        })
    }

    /// What a line names an entry as, between its number and its content.
    fn label(&self, entry: Entry<'_>) -> String {
        match entry {
            Entry::Message(message) => match &message.role {
                MessageRole::User => match &message.author {
                    None => "user".to_owned(),
                    Some(author) => sent_by(author),
                },
                MessageRole::Agent if message.status == MessageStatus::Streaming => {
                    "agent, still writing".to_owned()
                }
                MessageRole::Agent => "agent".to_owned(),
                MessageRole::Delegation(delegator)
                    if Some(delegator.session_id) == self.session.session.parent =>
                {
                    "delegation".to_owned()
                }
                MessageRole::Delegation(delegator) => {
                    format!("delegation from Session {}", delegator.session_id)
                }
            },
            Entry::Activity(activity) => activity_label(activity),
        }
    }

    /// The text of `entry` a line shows and a point counts characters of: a
    /// Message's content, or an Activity's one line.
    fn content(&self, entry: Entry<'_>) -> String {
        match entry {
            Entry::Message(message) => message.content.clone(),
            Entry::Activity(activity) => one_line(&self.summary(activity)),
        }
    }

    /// What an Activity's line says of it, before it is shortened.
    fn summary(&self, activity: &Activity) -> String {
        match activity {
            Activity::Command { command, .. } => command.clone(),
            Activity::FileChange { changes, .. } => changes
                .iter()
                .map(|change| file_change(change, |path| self.said_from_directory(path)))
                .collect::<Vec<_>>()
                .join(", "),
            Activity::ToolCall {
                name,
                server,
                input,
                ..
            } => {
                let tool = server
                    .as_ref()
                    .map_or_else(|| name.clone(), |server| format!("{server}/{name}"));
                if input.is_empty() {
                    tool
                } else {
                    format!("{tool} {input}")
                }
            }
            Activity::Subagent {
                name, description, ..
            } => format!("{name} — {description}"),
            Activity::Approval { approval, .. } => format!("may the Agent {}?", asks(approval)),
            Activity::Questionnaire { questionnaire, .. } => {
                let mut questions = questionnaire.questions.iter();
                let mut summary = questions
                    .next()
                    .map_or_else(String::new, |question| question.text.clone());
                let more = questions.count();
                if more > 0 {
                    summary.push_str(&format!(" (and {} more)", plural(more, "Question")));
                }
                summary
            }
            Activity::Status { text, .. } | Activity::Error { text, .. } => text.clone(),
            Activity::WatchOutcome {
                description,
                summary,
                ..
            } => match summary {
                Some(summary) => format!("{description} — {summary}"),
                None => description.clone(),
            },
            Activity::Compaction {
                instructions,
                before_tokens,
                after_tokens,
                error,
                ..
            } => {
                let mut parts = Vec::new();
                parts.extend(compaction_tokens(*before_tokens, *after_tokens));
                if let Some(instructions) = instructions {
                    parts.push(format!("asked to keep: {instructions}"));
                }
                parts.extend(error.clone());
                parts.join(" — ")
            }
            Activity::Reasoning { .. } => String::new(),
            Activity::Subsession { title, prompt, .. } => {
                format!("\"{title}\", first asked: {prompt}")
            }
        }
    }

    /// `path` said from the Session's own directory where it lies within it,
    /// and in full where it does not.
    fn said_from_directory(&self, path: &Path) -> String {
        path.strip_prefix(&self.session.session.execution_directory.path)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// Entry `number`, whole: everything Suru stores of it, its output
    /// included.
    fn whole(&self, number: EntryNumber) -> Result<String, ReadRefusal> {
        let turn = self.turn(number.turn)?;
        let Some(entry) = number
            .entry
            .checked_sub(1)
            .and_then(|index| turn.entries.get(index))
        else {
            return Err(ReadRefusal::NoSuchEntry {
                entry: number,
                entries: turn.entries.len(),
            });
        };
        let label = format!("{number} {}: ", self.label(*entry));
        let activity = match entry {
            Entry::Message(message) => {
                let mut whole = format!("{label}{}", message.content);
                if message.truncated {
                    whole.push_str(STORED_CUT);
                }
                return Ok(whole);
            }
            Entry::Activity(activity) => activity,
        };
        let mut whole = label;
        match activity {
            Activity::Command {
                command,
                cwd,
                output,
                output_truncated,
                ..
            } => {
                whole.push_str(command);
                if let Some(cwd) = cwd {
                    whole.push_str(&format!("\ndirectory: {}", cwd.display()));
                }
                push_output(&mut whole, output, *output_truncated);
            }
            Activity::FileChange { changes, .. } => {
                // Each file stands on a line of its own beneath the label.
                whole.pop();
                for change in changes {
                    whole.push('\n');
                    whole.push_str(&file_change(change, |path| path.display().to_string()));
                }
            }
            Activity::ToolCall {
                name,
                server,
                input,
                input_truncated,
                output,
                output_truncated,
                omitted_parts,
                ..
            } => {
                match server {
                    Some(server) => whole.push_str(&format!("{server}/{name}")),
                    None => whole.push_str(name),
                }
                whole.push_str(&format!("\ninput: {input}"));
                if *input_truncated {
                    whole.push_str(STORED_CUT);
                }
                push_output(&mut whole, output, *output_truncated);
                if *omitted_parts > 0 {
                    whole.push_str(&format!(
                        "\n[{} of the result that {} not text left out]",
                        plural(*omitted_parts as usize, "part"),
                        if *omitted_parts == 1 { "was" } else { "were" }
                    ));
                }
            }
            Activity::Subagent {
                name,
                description,
                model,
                session_id,
                duration_ms,
                ..
            } => {
                whole.push_str(&format!("{name} — {description}"));
                if let Some(model) = model {
                    whole.push_str(&format!("\nmodel: {model}"));
                }
                if let Some(worked) = duration_ms {
                    whole.push_str(&format!("\nworked {}", duration(*worked)));
                }
                whole.push_str(&format!(
                    "\nIts work is in its own Session, {session_id}; read that Session for it."
                ));
            }
            Activity::Approval {
                approval,
                detail_truncated,
                follow_up_error,
                ..
            } => {
                whole.push_str(&format!("may the Agent {}?", asks(approval)));
                if let Some(reason) = &approval.reason {
                    whole.push_str(&format!("\nreason: {reason}"));
                }
                if *detail_truncated {
                    whole.push_str(STORED_CUT);
                }
                if let Some(error) = follow_up_error {
                    whole.push_str(&format!(
                        "\nWhat the Provider did after the Decision failed: {error}"
                    ));
                }
            }
            Activity::Questionnaire {
                questionnaire,
                answer,
                ..
            } => {
                // Each Question stands on lines of its own beneath the label.
                whole.pop();
                push_questionnaire(&mut whole, questionnaire, answer.as_ref());
            }
            Activity::Status { text, .. } | Activity::Error { text, .. } => {
                whole.push_str(text);
            }
            Activity::WatchOutcome {
                description,
                summary,
                ..
            } => {
                whole.push_str(description);
                if let Some(summary) = summary {
                    whole.push('\n');
                    whole.push_str(summary);
                }
            }
            Activity::Compaction {
                instructions,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
                ..
            } => {
                // Each thing known of it stands on a line of its own beneath
                // the label.
                whole.pop();
                if let Some(tokens) = compaction_tokens(*before_tokens, *after_tokens) {
                    whole.push_str(&format!("\n{tokens}"));
                }
                if let Some(instructions) = instructions {
                    whole.push_str(&format!("\nasked to keep: {instructions}"));
                }
                if let Some(error) = error {
                    whole.push_str(&format!("\nerror: {error}"));
                }
                if let Some(summary) = summary {
                    whole.push_str("\nsummary:\n");
                    whole.push_str(summary);
                    if *summary_truncated {
                        whole.push_str(STORED_CUT);
                    }
                }
            }
            Activity::Subsession {
                session_id,
                title,
                prompt,
                ..
            } => {
                whole.push_str(title);
                whole.push_str(&format!("\nfirst asked: {prompt}"));
                whole.push_str(&format!(
                    "\nIt works in its own Session, {session_id}; read that Session for it."
                ));
            }
            Activity::Reasoning { .. } => {}
        }
        Ok(whole)
    }

    /// Where `activity` stands, numbered as a reading numbers it.
    fn number_of(&self, wanted: &Activity) -> Option<EntryNumber> {
        self.turns.iter().enumerate().find_map(|(turn, entries)| {
            entries
                .entries
                .iter()
                .enumerate()
                .find_map(|(entry, read)| {
                    matches!(read, Entry::Activity(activity) if activity.id() == wanted.id())
                        .then_some(EntryNumber {
                            turn: turn + 1,
                            entry: entry + 1,
                        })
                })
        })
    }

    /// Every Questionnaire an Answer would still reach: one waiting on its
    /// Answer, or offered again after its delivery was refused, in a Turn
    /// still working.
    fn open_questionnaires(&self) -> Vec<OpenQuestionnaire<'a>> {
        self.session
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Questionnaire {
                    questionnaire,
                    outcome,
                    turn_id,
                    ..
                } if outcome.is_answerable()
                    && self
                        .session
                        .turns
                        .iter()
                        .any(|turn| turn.id == *turn_id && turn.status == TurnStatus::Active) =>
                {
                    Some(OpenQuestionnaire {
                        entry: self.number_of(activity)?,
                        questionnaire,
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// Each Approval waiting on the user's Decision, as a sentence saying so
    /// and naming what it asks.
    fn awaited_decisions(&self) -> Vec<String> {
        self.session
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Approval { approval, .. }
                    if self.session.pending_approvals.contains(&approval.id) =>
                {
                    let number = self.number_of(activity)?;
                    let mut statement = format!(
                        "Approval {number} awaits the user's Decision on whether the Agent may {}.",
                        one_line(&asks(approval))
                    );
                    if let Some(reason) = &approval.reason {
                        statement.push_str(&format!(
                            " It gives as its reason \"{}\".",
                            one_line(reason)
                        ));
                    }
                    statement.push_str(
                        " Only the user can decide it, so tell them it is waiting on them.",
                    );
                    Some(statement)
                }
                _ => None,
            })
            .collect()
    }
}

/// One line of a reading: its number and label, the content a cap counts
/// and may cut the start of, and anything said after it.
struct Line {
    label: String,
    content: String,
    suffix: &'static str,
}

impl Line {
    fn whole(self) -> String {
        format!("{}{}{}", self.label, self.content, self.suffix)
    }

    /// The line keeping only the last `keep` characters of its content.
    fn tail(self, keep: usize) -> String {
        format!(
            "{}{CUT}{}{}",
            self.label,
            last_chars(&self.content, keep),
            self.suffix
        )
    }
}

/// What a File Change did to one file, its paths said as `path` says them:
/// from the Session's own directory in a line, in full in an entry read
/// whole.
fn file_change(change: &FileChange, path: impl Fn(&Path) -> String) -> String {
    match change {
        FileChange::Add { path: added } => format!("add {}", path(added)),
        FileChange::Delete { path: deleted } => format!("delete {}", path(deleted)),
        FileChange::Update {
            path: updated,
            moved_to: None,
        } => format!("update {}", path(updated)),
        FileChange::Update {
            path: updated,
            moved_to: Some(moved),
        } => format!("move {} to {}", path(updated), path(moved)),
    }
}

/// What an Activity's line names it as: its kind, and how it stands where
/// it settles through a lifecycle.
fn activity_label(activity: &Activity) -> String {
    let stood = |status: ActivityStatus| match status {
        ActivityStatus::Active => "running",
        ActivityStatus::Completed => "completed",
        ActivityStatus::Failed => "failed",
        ActivityStatus::Interrupted => "interrupted",
    };
    match activity {
        Activity::Command {
            status,
            exit_status,
            ..
        } => match exit_status {
            Some(code) if *code != 0 => format!("command [{}, exit {code}]", stood(*status)),
            _ => format!("command [{}]", stood(*status)),
        },
        Activity::FileChange { status, .. } => format!("file change [{}]", stood(*status)),
        Activity::ToolCall { status, .. } => format!("tool call [{}]", stood(*status)),
        Activity::Subagent {
            status, session_id, ..
        } => format!("subagent [{}, Session {session_id}]", stood(*status)),
        Activity::Approval {
            outcome, decision, ..
        } => format!("approval [{}]", approval_outcome(*outcome, *decision)),
        Activity::Questionnaire { outcome, .. } => {
            format!("questionnaire [{}]", questionnaire_outcome(*outcome))
        }
        Activity::Status { .. } => "status".to_owned(),
        Activity::Error { .. } => "error".to_owned(),
        Activity::WatchOutcome { status, .. } => format!(
            "watch outcome [{}]",
            match status {
                WatchOutcomeStatus::Completed => "completed",
                WatchOutcomeStatus::Failed => "failed",
                WatchOutcomeStatus::Stopped => "stopped",
            }
        ),
        Activity::Compaction {
            status, trigger, ..
        } => match trigger {
            CompactionTrigger::Manual => format!("compaction [{}]", stood(*status)),
            CompactionTrigger::Automatic => format!("compaction [{}, automatic]", stood(*status)),
        },
        Activity::Reasoning { .. } => "reasoning".to_owned(),
        Activity::Subsession { session_id, .. } => format!("subsession [Session {session_id}]"),
    }
}

/// How a Compaction changed the context's size, where either side is known.
fn compaction_tokens(before: Option<u64>, after: Option<u64>) -> Option<String> {
    match (before, after) {
        (Some(before), Some(after)) => Some(format!("context from {before} to {after} tokens")),
        (Some(before), None) => Some(format!("context from {before} tokens")),
        (None, Some(after)) => Some(format!("context to {after} tokens")),
        (None, None) => None,
    }
}

fn approval_outcome(outcome: ApprovalOutcome, decision: Option<Decision>) -> String {
    match outcome {
        ApprovalOutcome::Pending | ApprovalOutcome::SubmissionRejected => {
            "awaiting the user's Decision".to_owned()
        }
        ApprovalOutcome::Submitting => "Decision being delivered".to_owned(),
        ApprovalOutcome::Decided => match decision {
            Some(Decision::Accept) => "accepted".to_owned(),
            Some(Decision::AcceptForSession) => "accepted for the Session".to_owned(),
            Some(Decision::Decline) => "declined".to_owned(),
            Some(Decision::DeclineAndInterrupt) => "declined and interrupted".to_owned(),
            None => "decided".to_owned(),
        },
        ApprovalOutcome::Withdrawn => "withdrawn".to_owned(),
        ApprovalOutcome::TurnEnded => "its Turn ended".to_owned(),
        ApprovalOutcome::Unavailable => "unavailable".to_owned(),
        ApprovalOutcome::DeliveryUncertain => "Decision's delivery uncertain".to_owned(),
    }
}

const fn questionnaire_outcome(outcome: QuestionnaireOutcome) -> &'static str {
    match outcome {
        QuestionnaireOutcome::Pending | QuestionnaireOutcome::SubmissionRejected => {
            "awaiting an Answer"
        }
        QuestionnaireOutcome::Submitting => "Answer being delivered",
        QuestionnaireOutcome::Answered => "answered",
        QuestionnaireOutcome::Declined => "declined",
        QuestionnaireOutcome::Withdrawn => "withdrawn",
        QuestionnaireOutcome::TurnEnded => "its Turn ended",
        QuestionnaireOutcome::Unavailable => "unavailable",
        QuestionnaireOutcome::DeliveryUncertain => "Answer's delivery uncertain",
    }
}

/// What an Approval asks the Agent be let do, finishing "may the Agent …".
fn asks(approval: &Approval) -> String {
    match &approval.subject {
        ApprovalSubject::Command { command, cwd, .. } => match cwd {
            Some(cwd) => format!("run `{command}` in {}", cwd.display()),
            None => format!("run `{command}`"),
        },
        ApprovalSubject::FileChange { paths, grant_root } => {
            let mut asks = format!(
                "change {}",
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if let Some(root) = grant_root {
                asks.push_str(&format!(" and anything under {}", root.display()));
            }
            asks
        }
        ApprovalSubject::Read { path } => format!("read {}", path.display()),
        ApprovalSubject::Network { host_or_url } => format!("reach {host_or_url}"),
        ApprovalSubject::PermissionGrant { profile } => format!("be granted {profile}"),
        ApprovalSubject::OtherTool { name, input } => format!("use {name} with {input}"),
    }
}

/// Each Subagent's Session beneath `session` holding an Intervention, as a
/// sentence sending the reader there.
fn subagent_interventions(session: &SessionSnapshot) -> Vec<String> {
    session
        .subagent_interventions
        .iter()
        .filter_map(|owed| {
            let owes = [
                (!owed.pending_questionnaires.is_empty()).then(|| {
                    format!(
                        "{} awaiting an Answer",
                        plural(owed.pending_questionnaires.len(), "Questionnaire")
                    )
                }),
                (!owed.pending_approvals.is_empty()).then(|| {
                    format!(
                        "{} awaiting the user's Decision",
                        plural(owed.pending_approvals.len(), "Approval")
                    )
                }),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            (!owes.is_empty()).then(|| {
                format!(
                    "A Subagent's Session beneath this one, {}, holds {}; read that Session for {}.",
                    owed.session_id,
                    joined(&owes),
                    if owes.len() == 1
                        && owed.pending_questionnaires.len() + owed.pending_approvals.len() == 1
                    {
                        "it"
                    } else {
                        "them"
                    }
                )
            })
        })
        .collect()
}

/// Adds an Activity's output to its whole reading, saying where Suru's cap
/// cut it short.
fn push_output(whole: &mut String, output: &str, truncated: bool) {
    if output.is_empty() {
        whole.push_str("\nno output");
    } else {
        whole.push_str("\noutput:\n");
        whole.push_str(output);
    }
    if truncated {
        whole.push_str(STORED_CUT);
    }
}

/// Adds a Questionnaire's Questions, their choices, and the Answer it was
/// given, to its whole reading. A secret Answer is said to be one and no
/// more, as Suru stores it.
fn push_questionnaire(whole: &mut String, questionnaire: &Questionnaire, answer: Option<&Answer>) {
    for (index, question) in questionnaire.questions.iter().enumerate() {
        whole.push_str(&format!("\nQuestion {} ({}): ", index + 1, question.id));
        if let Some(title) = &question.title {
            whole.push_str(&format!("{title}: "));
        }
        whole.push_str(&question.text);
        whole.push_str(&format!(
            "\n  takes {}{}{}",
            accepted_answers(question),
            if question.required {
                "; required"
            } else {
                "; may be left unanswered"
            },
            if question.secret {
                "; its Answer is secret"
            } else {
                ""
            }
        ));
        for choice in &question.choices {
            whole.push_str(&format!("\n  - {}: {}", choice.id, choice.label));
            if let Some(description) = &choice.description {
                whole.push_str(&format!(" — {description}"));
            }
            if choice.recommended {
                whole.push_str(" (recommended)");
            }
        }
    }
    let Some(answer) = answer else {
        return;
    };
    for (index, (question, given)) in questionnaire
        .questions
        .iter()
        .zip(&answer.questions)
        .enumerate()
    {
        let label = |chosen: &[String]| {
            chosen
                .iter()
                .map(|id| {
                    question
                        .choices
                        .iter()
                        .find(|choice| &choice.id == id)
                        .map_or_else(|| id.clone(), |choice| choice.label.clone())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let said = match given {
            QuestionAnswer::Selected { choices } => label(choices),
            QuestionAnswer::SelectedWithFreeform { choices, text } => {
                format!("{}; {text}", label(choices))
            }
            QuestionAnswer::Freeform { text } => text.clone(),
            QuestionAnswer::Omitted => "left unanswered".to_owned(),
            QuestionAnswer::SecretAnswered => "answered in secret".to_owned(),
        };
        whole.push_str(&format!("\nAnswer {}: {said}", index + 1));
    }
}

/// What a line names a user Message as that was sent on the user's behalf
/// rather than by the user: who sent it, so a reader never takes their words
/// for the user's. A Sidekick is named by its Session's Title as it stood
/// when it sent the Prompt, and by that Session, which a reader may read.
fn sent_by(author: &Author) -> String {
    match author {
        Author::Sidekick { session_id, title } if title.trim().is_empty() => {
            format!("sent by a Sidekick (Session {session_id})")
        }
        Author::Sidekick { session_id, title } => format!(
            "sent by Sidekick \"{}\" (Session {session_id})",
            shortened(title, TITLE_CHARS)
        ),
    }
}

/// The Answers `question` accepts besides leaving it unanswered, as
/// [`Question::accepts`] reads an Answer: choices only where it offers
/// some, free text only where it takes it, and both at once only where it
/// takes both together.
fn accepted_answers(question: &Question) -> String {
    let offers_choices = !question.choices.is_empty();
    let choices = if question.multiple {
        "one or more of the choices"
    } else {
        "one of the choices"
    };
    let mut accepted = Vec::new();
    if offers_choices {
        accepted.push(choices.to_owned());
    }
    if question.freeform {
        accepted.push("free text".to_owned());
    }
    if offers_choices && question.freeform && question.combine_freeform {
        accepted.push(format!(
            "{choices} with free text beside {}",
            if question.multiple { "them" } else { "it" }
        ));
    }
    match accepted.as_slice() {
        [] => "no Answer".to_owned(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// `text` as one line of at most [`LINE_CHARS`] characters; see
/// [`shortened`].
fn one_line(text: &str) -> String {
    shortened(text, LINE_CHARS)
}

/// `text` as one line of at most `chars` characters: its first line, ending
/// in an ellipsis where anything of it was left out.
fn shortened(text: &str, chars: usize) -> String {
    let text = text.trim();
    let first = text.lines().next().unwrap_or_default();
    let shortened = first.chars().count() > chars || first.len() < text.len();
    if !shortened {
        return first.to_owned();
    }
    let mut line = first_chars(first, chars - 1).trim_end().to_owned();
    line.push('…');
    line
}

/// The first `chars` characters of `text`, or all of it.
fn first_chars(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(at, _)| &text[..at])
}

/// The last `chars` characters of `text`, or all of it.
fn last_chars(text: &str, chars: usize) -> &str {
    let skip = text.chars().count().saturating_sub(chars);
    &text[first_chars(text, skip).len()..]
}

/// `count` of `noun`, said as a reader would.
fn plural(count: usize, noun: &str) -> String {
    match (count, noun) {
        (1, noun) => format!("1 {noun}"),
        (count, "Activity") => format!("{count} Activities"),
        (count, noun) => format!("{count} {noun}s"),
    }
}

/// `parts` joined as a sentence lists them.
fn joined(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `at` as a heading says it: an RFC 3339 moment in UTC, to the second.
fn spelled(at: SessionTimestamp) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at.0) * 1_000_000)
        .ok()
        .and_then(|moment| {
            moment
                .format(format_description!(
                    "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
                ))
                .ok()
        })
        .unwrap_or_else(|| at.0.to_string())
}

/// How long something worked, to the second, as a heading says it.
fn duration(ms: u64) -> String {
    let seconds = ms / 1_000;
    let (hours, minutes, seconds) = (seconds / 3_600, seconds / 60 % 60, seconds % 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, minutes) => format!("{minutes}m {seconds}s"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::protocol::{
        ActivityId, ApprovalId, Delegator, MessageId, ModelId, PromptId, QuestionnaireId, Session,
        SessionId, SessionRevision, SubagentInterventions, TurnId, Workspace,
    };
    use crate::questionnaire::{Question, QuestionChoice};

    /// Where the fixture Session works, rooted as each platform roots a path.
    #[cfg(windows)]
    const ROOT: &str = r"C:\work\ledger";
    #[cfg(not(windows))]
    const ROOT: &str = "/work/ledger";

    /// 2026-10-01T09:30:00Z, when every fixture Turn begins.
    const BEGAN: u64 = 1_790_847_000_000;

    /// A Session built entry by entry, as the Server would have stored it.
    struct Fixture(SessionSnapshot);

    impl Fixture {
        fn new() -> Self {
            Self(SessionSnapshot {
                title: "Fix the flaky test".to_owned(),
                icon: None,
                session: Session::for_tests(Workspace::directory(PathBuf::from(ROOT))),
                revision: SessionRevision::INITIAL,
                prompts: Vec::new(),
                turns: Vec::new(),
                messages: Vec::new(),
                activities: Vec::new(),
                transcript: Vec::new(),
                subagent_usage: None,
                total_cost: None,
                own_cost: None,
                subagent_interventions: Vec::new(),
                pending_approvals: Vec::new(),
                submitting_approvals: Vec::new(),
                pending_approvals_revision: SessionRevision(0),
                watches: Vec::new(),
                attachments: Vec::new(),
                waiting_on_subagents: None,
            })
        }

        /// A Subagent's Session, beneath `parent`.
        fn subagent_of(parent: SessionId) -> Self {
            let mut fixture = Self::new();
            fixture.0.session.parent = Some(parent);
            fixture
        }

        /// Begins a Turn a Prompt opened, standing as `status`: settled two
        /// minutes and three seconds after it began, unless it still works.
        fn turn(&mut self, status: TurnStatus) -> TurnId {
            self.begin(status, Some(PromptId::new()))
        }

        /// Begins a Turn nothing asked for.
        fn continuation(&mut self, status: TurnStatus) -> TurnId {
            self.begin(status, None)
        }

        fn begin(&mut self, status: TurnStatus, prompt_id: Option<PromptId>) -> TurnId {
            let id = TurnId::new();
            self.0.turns.push(Turn {
                prompt_id,
                status,
                started_at: Some(SessionTimestamp(BEGAN)),
                settled_at: status
                    .is_terminal()
                    .then_some(SessionTimestamp(BEGAN + 123_000)),
                ..Turn::unprompted(None)
            });
            self.0.turns.last_mut().expect("just pushed").id = id;
            id
        }

        fn message(&mut self, turn_id: TurnId, role: MessageRole, content: &str) -> MessageId {
            self.authored(turn_id, role, None, content)
        }

        /// A Message `author` sent on the user's behalf, where it names one.
        fn authored(
            &mut self,
            turn_id: TurnId,
            role: MessageRole,
            author: Option<Author>,
            content: &str,
        ) -> MessageId {
            let id = MessageId::new();
            self.0.messages.push(Message {
                id,
                turn_id,
                role,
                status: MessageStatus::Completed,
                content: content.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
                truncated: false,
                author,
            });
            self.0
                .transcript
                .push(TranscriptItem::Message { message_id: id });
            id
        }

        fn user(&mut self, turn_id: TurnId, content: &str) {
            self.message(turn_id, MessageRole::User, content);
        }

        fn agent(&mut self, turn_id: TurnId, content: &str) {
            self.message(turn_id, MessageRole::Agent, content);
        }

        fn activity(&mut self, activity: Activity) {
            self.0.transcript.push(TranscriptItem::Activity {
                activity_id: activity.id(),
            });
            self.0.activities.push(activity);
        }

        fn command(&mut self, turn_id: TurnId, command: &str, output: &str) {
            self.activity(Activity::Command {
                id: ActivityId::new(),
                turn_id,
                status: ActivityStatus::Completed,
                command: command.to_owned(),
                cwd: Some(PathBuf::from(ROOT)),
                output: output.to_owned(),
                output_truncated: false,
                exit_status: Some(0),
            });
        }

        fn reasoning(&mut self, turn_id: TurnId, content: &str) {
            self.activity(Activity::Reasoning {
                id: ActivityId::new(),
                turn_id,
                status: ActivityStatus::Completed,
                title: Some("Secret plans".to_owned()),
                content: content.to_owned(),
                content_truncated: false,
                duration_ms: Some(1_000),
            });
        }

        fn read(&self, request: ReadRequest) -> SessionReading<'_> {
            read(&self.0, &request).expect("the read names what the Session holds")
        }

        fn window(&self, window: Window) -> SessionReading<'_> {
            self.read(ReadRequest::Window(window))
        }

        fn refused(&self, request: ReadRequest) -> ReadRefusal {
            read(&self.0, &request).expect_err("the read names what the Session does not hold")
        }
    }

    fn before(spelled: &str) -> Option<Position> {
        Some(spelled.parse().expect("a point"))
    }

    fn entry(spelled: &str) -> ReadRequest {
        ReadRequest::Entry(spelled.parse().expect("an entry's number"))
    }

    /// Three Turns of work: the last one reading a file, running the tests,
    /// thinking, and saying what it found along the way and at the end.
    fn three_turns() -> Fixture {
        let mut fixture = Fixture::new();
        let first = fixture.turn(TurnStatus::Completed);
        fixture.user(first, "Look at the ledger.");
        fixture.agent(first, "The ledger has a flaky test.");
        let second = fixture.turn(TurnStatus::Completed);
        fixture.user(second, "Which one?");
        fixture.agent(second, "ledger::tests::sync.");
        let third = fixture.turn(TurnStatus::Completed);
        fixture.user(third, "Fix it.");
        fixture.reasoning(third, "The user must never see this Reasoning.");
        fixture.agent(third, "Let me run the tests first.");
        fixture.command(third, "cargo test -p ledger", "test sync ... FAILED");
        fixture.activity(Activity::FileChange {
            id: ActivityId::new(),
            turn_id: third,
            status: ActivityStatus::Completed,
            changes: vec![FileChange::Update {
                path: PathBuf::from(ROOT).join("src").join("sync.rs"),
                moved_to: None,
            }],
        });
        fixture.activity(Activity::ToolCall {
            id: ActivityId::new(),
            turn_id: third,
            status: ActivityStatus::Completed,
            name: "search_issues".to_owned(),
            server: Some("github".to_owned()),
            input: "{\"q\":\"flaky\"}".to_owned(),
            input_truncated: false,
            output: "No issues found.".to_owned(),
            output_truncated: false,
            omitted_parts: 0,
        });
        fixture.activity(Activity::Subagent {
            id: ActivityId::new(),
            turn_id: third,
            status: ActivityStatus::Completed,
            name: "Scout".to_owned(),
            description: "Check the other tests".to_owned(),
            model: None,
            session_id: SessionId::from_uuid(uuid::Uuid::nil()),
            brokered: true,
            duration_ms: Some(4_000),
        });
        fixture.agent(third, "Fixed the race in sync; the tests pass.");
        fixture
    }

    #[test]
    fn a_default_read_is_the_latest_turns_user_message_and_final_agent_message() {
        let fixture = three_turns();
        let reading = fixture.window(Window::default());

        assert_eq!(
            reading.transcript,
            "[Turn 3 of 3 · completed at 2026-10-01T09:32:03Z after 2m 3s · 1 agent Message and \
             4 Activities not shown]\n\
             3.1 user: Fix it.\n\
             3.7 agent: Fixed the race in sync; the tests pass."
        );
        assert!(reading.transcript.chars().count() <= DEFAULT_MAX_CHARS);
        assert_eq!(reading.before, before("3"));
        assert_eq!(
            reading.earlier.as_deref(),
            Some("Left out before this: Turns 1–2. Call again with before \"3\" to read on.")
        );
        assert!(reading.questionnaires.is_empty() && reading.approvals.is_empty());
    }

    #[test]
    fn activities_add_every_message_and_one_line_per_activity_without_its_output() {
        let fixture = three_turns();
        let reading = fixture.window(Window {
            detail: Detail::Activities,
            ..Window::default()
        });

        let subagent = SessionId::from_uuid(uuid::Uuid::nil());
        let changed = Path::new("src").join("sync.rs");
        assert_eq!(
            reading.transcript,
            format!(
                "[Turn 3 of 3 · completed at 2026-10-01T09:32:03Z after 2m 3s]\n\
                 3.1 user: Fix it.\n\
                 3.2 agent: Let me run the tests first.\n\
                 3.3 command [completed]: cargo test -p ledger\n\
                 3.4 file change [completed]: update {}\n\
                 3.5 tool call [completed]: github/search_issues {{\"q\":\"flaky\"}}\n\
                 3.6 subagent [completed, Session {subagent}]: Scout — Check the other tests\n\
                 3.7 agent: Fixed the race in sync; the tests pass.",
                changed.display()
            )
        );
        assert!(
            !reading.transcript.contains("FAILED") && !reading.transcript.contains("No issues"),
            "no Activity's output is shown"
        );
    }

    #[test]
    fn reasoning_never_appears_at_any_detail_nor_whole() {
        let fixture = three_turns();
        for detail in [Detail::Messages, Detail::Activities] {
            for turns in [1, 3] {
                let reading = fixture.window(Window {
                    turns,
                    detail,
                    ..Window::default()
                });
                assert!(
                    !reading.transcript.contains("Reasoning")
                        && !reading.transcript.contains("Secret plans")
                        && !reading.transcript.contains("reasoning"),
                    "{detail:?}: {}",
                    reading.transcript
                );
            }
        }
        for number in 1..=7 {
            let whole = fixture.read(entry(&format!("3.{number}"))).transcript;
            assert!(!whole.contains("Secret plans"), "{whole}");
        }
        assert_eq!(
            fixture.refused(entry("3.8")),
            ReadRefusal::NoSuchEntry {
                entry: "3.8".parse().expect("a number"),
                entries: 7
            },
            "Reasoning takes no number, so none can name it"
        );
    }

    #[test]
    fn turns_reads_further_back_and_says_nothing_is_left_out_once_it_reaches_the_start() {
        let fixture = three_turns();
        let two = fixture.window(Window {
            turns: 2,
            ..Window::default()
        });
        assert!(
            two.transcript.starts_with("[Turn 2 of 3"),
            "{}",
            two.transcript
        );
        assert!(
            two.transcript
                .contains("2.1 user: Which one?\n2.2 agent: ledger::tests::sync.")
        );
        assert_eq!(two.before, before("2"));
        assert_eq!(
            two.earlier.as_deref(),
            Some("Left out before this: Turn 1. Call again with before \"2\" to read on.")
        );

        let all = fixture.window(Window {
            turns: 50,
            ..Window::default()
        });
        assert!(
            all.transcript.starts_with("[Turn 1 of 3"),
            "{}",
            all.transcript
        );
        assert_eq!((all.before, all.earlier), (None, None));
    }

    #[test]
    fn before_reads_the_turns_before_a_point() {
        let fixture = three_turns();
        let reading = fixture.window(Window {
            before: before("3"),
            ..Window::default()
        });
        assert!(reading.transcript.starts_with("[Turn 2 of 3"));
        assert!(!reading.transcript.contains("Turn 3"));
        assert_eq!(reading.before, before("2"));

        let start = fixture.window(Window {
            before: before("1"),
            ..Window::default()
        });
        assert_eq!(start.transcript, "", "nothing comes before the first Turn");
        assert_eq!((start.before, start.earlier), (None, None));

        let within = fixture.window(Window {
            before: before("3.3"),
            detail: Detail::Activities,
            ..Window::default()
        });
        assert!(
            within
                .transcript
                .ends_with("3.1 user: Fix it.\n3.2 agent: Let me run the tests first."),
            "{}",
            within.transcript
        );
    }

    #[test]
    fn the_cap_keeps_the_end_and_says_where_to_read_on_from() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        fixture.user(turn, "Summarise the ledger.");
        let long = (0..400)
            .map(|line| format!("Line {line} of the summary."))
            .collect::<Vec<_>>()
            .join("\n");
        fixture.agent(turn, &long);

        let reading = fixture.window(Window::default());
        assert!(reading.transcript.starts_with("[Turn 1 of 1 · completed"));
        assert!(reading.transcript.contains("\n1.2 agent: […]"));
        assert!(reading.transcript.ends_with("Line 399 of the summary."));
        let before = reading.before.expect("the cut is stated");
        let shown = reading
            .transcript
            .split_once("1.2 agent: […]")
            .expect("the cut line")
            .1;
        assert_eq!(
            shown.chars().count(),
            DEFAULT_MAX_CHARS,
            "the cap counts what entries say, and the newest is kept to it"
        );
        assert_eq!(said(&reading.transcript), DEFAULT_MAX_CHARS);
        assert_eq!(
            before.to_string(),
            format!("1.2.{}", long.chars().count() - shown.chars().count())
        );
        assert_eq!(
            reading.earlier,
            Some(format!(
                "Left out before this: the first {} characters of 1.2 and entry 1.1. Call again \
                 with before \"{before}\" to read on, or with item \"1.2\" to read that entry whole.",
                long.chars().count() - shown.chars().count()
            ))
        );
    }

    /// Reading back from each answer's `before` until none is given shows
    /// every character of every entry exactly once, each cap kept, whatever
    /// the cap, the Turns asked for and the detail — multi-byte text cut
    /// wherever a cap falls.
    #[test]
    fn reading_on_from_each_point_given_loses_and_repeats_nothing() {
        let mut fixture = three_turns();
        let fourth = fixture.turn(TurnStatus::Completed);
        fixture.user(fourth, &"Tell me everything. ".repeat(60));
        fixture.agent(
            fourth,
            &"Here is everything 🦀 — ünïcödé, 漢字. ".repeat(150),
        );

        for detail in [Detail::Messages, Detail::Activities] {
            let whole = fixture.window(Window {
                turns: usize::MAX,
                max_chars: usize::MAX,
                detail,
                ..Window::default()
            });
            assert_eq!(whole.before, None);
            for max_chars in [1, 37, 777, DEFAULT_MAX_CHARS] {
                for turns in [1, 2] {
                    let mut pieces = Vec::new();
                    let mut from = None;
                    loop {
                        let reading = fixture.window(Window {
                            turns,
                            max_chars,
                            before: from,
                            detail,
                        });
                        assert!(said(&reading.transcript) <= max_chars);
                        assert_eq!(reading.before.is_some(), reading.earlier.is_some());
                        assert!(
                            reading.before.is_none() || reading.before != from,
                            "every read moves on"
                        );
                        pieces.push(reading.transcript);
                        match reading.before {
                            Some(next) => from = Some(next),
                            None => break,
                        }
                    }
                    assert_eq!(
                        stitched(&pieces),
                        entry_text(&whole.transcript),
                        "{detail:?}, {max_chars} characters, {turns} Turns at a time"
                    );
                }
            }
        }
    }

    /// How many characters of what its entries say `transcript` shows: all
    /// of it but its headings, the number and label that begin each entry's
    /// line, and the mark of a cut.
    fn said(transcript: &str) -> usize {
        entry_text(transcript)
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(|line| match labelled(line) {
                Some(content) => content.trim_start_matches(CUT).chars().count(),
                // A line of its own within a Message, after the line break
                // that is the Message's too.
                None => line.chars().count() + 1,
            })
            .sum()
    }

    /// What an entry's line says after its number and label, where `line`
    /// begins one.
    fn labelled(line: &str) -> Option<&str> {
        let (number, rest) = line.split_once(' ')?;
        number.parse::<EntryNumber>().ok()?;
        rest.split_once(": ").map(|(_, content)| content)
    }

    /// What `transcript` says of its entries: everything but its headings.
    fn entry_text(transcript: &str) -> String {
        transcript
            .lines()
            .filter(|line| !line.starts_with("[Turn "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The entries `pieces` show — each the answer of a read back from the
    /// one before it — in Transcript order, an entry a cap cut joined to the
    /// rest of it the next read showed.
    fn stitched(pieces: &[String]) -> String {
        let mut stitched = String::new();
        for piece in pieces.iter().rev().map(|piece| entry_text(piece)) {
            if piece.is_empty() {
                continue;
            }
            let cut = piece
                .find(CUT)
                .filter(|at| piece[..*at].ends_with(": ") && !piece[..*at].contains('\n'));
            match cut {
                Some(at) => stitched.push_str(&piece[at + CUT.len()..]),
                None => {
                    if !stitched.is_empty() {
                        stitched.push('\n');
                    }
                    stitched.push_str(&piece);
                }
            }
        }
        stitched
    }

    #[test]
    fn a_cut_counts_characters_and_never_splits_one() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        let text = "🦀漢ü".repeat(400);
        fixture.agent(turn, &text);

        let reading = fixture.window(Window {
            max_chars: 7,
            ..Window::default()
        });
        let shown = reading
            .transcript
            .split_once(CUT)
            .expect("the line lost its start")
            .1;
        assert_eq!(
            shown.chars().count(),
            7,
            "a cut fills the cap exactly, however many bytes a character takes"
        );
        assert!(text.ends_with(shown));
        let position = reading.before.expect("the cut is stated");
        assert_eq!(
            position.to_string(),
            format!("1.1.{}", text.chars().count() - shown.chars().count())
        );
        let rest = fixture.window(Window {
            before: Some(position),
            max_chars: usize::MAX,
            ..Window::default()
        });
        let head = rest
            .transcript
            .split_once("1.1 agent: ")
            .expect("the rest of the entry")
            .1;
        assert_eq!(format!("{head}{shown}"), text);
    }

    #[test]
    fn an_entry_read_whole_is_never_cut_and_holds_its_output() {
        let mut fixture = three_turns();
        let turn = fixture.turn(TurnStatus::Failed);
        let long = "x".repeat(10 * DEFAULT_MAX_CHARS);
        fixture.agent(turn, &long);
        fixture.activity(Activity::Error {
            id: ActivityId::new(),
            turn_id: turn,
            text: "The build broke.".to_owned(),
        });

        assert_eq!(
            fixture.read(entry("4.1")).transcript,
            format!("4.1 agent: {long}")
        );
        let reading = fixture.read(entry("3.3"));
        assert_eq!(
            reading.transcript,
            format!(
                "3.3 command [completed]: cargo test -p ledger\ndirectory: {ROOT}\noutput:\n\
                 test sync ... FAILED"
            )
        );
        assert_eq!((reading.before, reading.earlier), (None, None));
        assert_eq!(
            fixture.read(entry("3.5")).transcript,
            "3.5 tool call [completed]: github/search_issues\ninput: {\"q\":\"flaky\"}\noutput:\n\
             No issues found."
        );
        assert!(
            fixture
                .window(Window::default())
                .transcript
                .starts_with("[Turn 4 of 4 · failed at 2026-10-01T09:32:03Z after 2m 3s: The build broke. · 1 Activity not shown]"),
            "a failed Turn's heading says what it failed with"
        );
    }

    #[test]
    fn a_compaction_reads_as_its_change_in_size_and_whole_as_its_summary() {
        let mut fixture = Fixture::new();
        let turn = fixture.continuation(TurnStatus::Completed);
        fixture.0.turns[0].compaction_requested = true;
        fixture.activity(Activity::Compaction {
            id: ActivityId::new(),
            turn_id: turn,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Manual,
            instructions: Some("the parser work".to_owned()),
            before_tokens: Some(120_000),
            after_tokens: Some(9_000),
            error: None,
            summary: Some("We rewrote the lexer.".to_owned()),
            summary_truncated: true,
        });
        let failed = fixture.continuation(TurnStatus::Completed);
        fixture.activity(Activity::Compaction {
            id: ActivityId::new(),
            turn_id: failed,
            status: ActivityStatus::Failed,
            trigger: CompactionTrigger::Automatic,
            instructions: None,
            before_tokens: None,
            after_tokens: None,
            error: Some("The summary was empty.".to_owned()),
            summary: None,
            summary_truncated: false,
        });

        let reading = fixture.window(Window {
            detail: Detail::Activities,
            turns: 2,
            ..Window::default()
        });
        assert!(
            reading.transcript.contains(
                "\n1.1 compaction [completed]: context from 120000 to 9000 tokens — asked to \
                 keep: the parser work\n"
            ),
            "{}",
            reading.transcript
        );
        assert!(
            reading
                .transcript
                .ends_with("\n2.1 compaction [failed, automatic]: The summary was empty."),
            "{}",
            reading.transcript
        );
        assert!(
            !reading
                .transcript
                .starts_with("[Turn 1 of 2 · Continuation"),
            "a Turn the user's request for a Compaction began is no Continuation"
        );
        assert_eq!(
            fixture.read(entry("1.1")).transcript,
            "1.1 compaction [completed]:\ncontext from 120000 to 9000 tokens\nasked to keep: the \
             parser work\nsummary:\nWe rewrote the lexer. [cut short when stored]"
        );
    }

    #[test]
    fn what_suru_cut_short_when_it_stored_an_entry_is_said() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        let message = fixture.message(turn, MessageRole::Agent, "The start of it");
        fixture
            .0
            .messages
            .iter_mut()
            .find(|stored| stored.id == message)
            .expect("the Message")
            .truncated = true;

        let expected = "1.1 agent: The start of it [cut short when stored]";
        assert!(
            fixture
                .window(Window::default())
                .transcript
                .ends_with(expected)
        );
        assert_eq!(fixture.read(entry("1.1")).transcript, expected);
    }

    #[test]
    fn an_activitys_line_is_one_line_and_says_where_it_was_shortened() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        let script = format!("cat <<'EOF'\n{}\nEOF", "y".repeat(500));
        fixture.command(turn, &script, "");
        let long = format!("cargo {}", "z".repeat(500));
        fixture.command(turn, &long, "");

        let reading = fixture.window(Window {
            detail: Detail::Activities,
            ..Window::default()
        });
        assert!(
            reading
                .transcript
                .contains("\n1.1 command [completed]: cat <<'EOF'…\n"),
            "{}",
            reading.transcript
        );
        let last = reading.transcript.lines().last().expect("a line");
        assert!(last.ends_with('…'));
        assert_eq!(
            last.trim_start_matches("1.2 command [completed]: ")
                .chars()
                .count(),
            LINE_CHARS
        );
        assert!(
            fixture.read(entry("1.2")).transcript.contains(&long),
            "read whole, it is all there"
        );
    }

    #[test]
    fn an_open_questionnaire_is_given_with_its_number_and_a_settled_one_is_not() {
        let mut fixture = Fixture::new();
        let settled = fixture.turn(TurnStatus::Completed);
        let question = |id: &str| Question {
            id: id.to_owned(),
            title: Some("Machine".to_owned()),
            text: "Where should the tests run?".to_owned(),
            choices: vec![QuestionChoice {
                id: "staging".to_owned(),
                label: "Staging".to_owned(),
                description: Some("The shared machine".to_owned()),
                recommended: true,
            }],
            multiple: false,
            freeform: true,
            combine_freeform: false,
            secret: false,
            required: true,
        };
        let questionnaire = |turn_id, outcome| Activity::Questionnaire {
            id: ActivityId::new(),
            turn_id,
            questionnaire: Questionnaire {
                id: QuestionnaireId::new(),
                questions: vec![question("machine")],
            },
            outcome,
            answer: None,
        };
        fixture.activity(questionnaire(settled, QuestionnaireOutcome::Answered));
        let working = fixture.turn(TurnStatus::Active);
        fixture.user(working, "Run the tests.");
        fixture.activity(questionnaire(working, QuestionnaireOutcome::Pending));

        let reading = fixture.window(Window::default());
        let [open] = reading.questionnaires.as_slice() else {
            panic!("one Questionnaire is open: {:?}", reading.questionnaires);
        };
        assert_eq!(open.entry.to_string(), "2.2");
        assert_eq!(open.questionnaire.questions, vec![question("machine")]);
        assert!(
            reading.transcript.starts_with(
                "[Turn 2 of 2 · working since 2026-10-01T09:30:00Z · 1 Activity not shown]"
            ),
            "{}",
            reading.transcript
        );
        assert_eq!(
            fixture.read(entry("2.2")).transcript,
            "2.2 questionnaire [awaiting an Answer]:\nQuestion 1 (machine): Machine: Where should \
             the tests run?\n  takes one of the choices or free text; required\n  - staging: \
             Staging — The shared machine (recommended)"
        );
    }

    #[test]
    fn a_pending_approval_is_said_to_await_the_users_decision_with_what_it_asks() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Active);
        fixture.user(turn, "Run the tests.");
        let approval = |command: &str, outcome, decision| Activity::Approval {
            id: ActivityId::new(),
            turn_id: turn,
            approval: Approval {
                id: ApprovalId::new(),
                subject: ApprovalSubject::Command {
                    command: command.to_owned(),
                    cwd: None,
                    actions: Vec::new(),
                },
                reason: Some("The tests need the network.".to_owned()),
            },
            tool_activity_id: None,
            detail_truncated: false,
            outcome,
            decision,
            follow_up_error: None,
        };
        fixture.activity(approval(
            "cargo build",
            ApprovalOutcome::Decided,
            Some(Decision::Accept),
        ));
        let pending = approval("cargo nextest run", ApprovalOutcome::Pending, None);
        let Activity::Approval {
            approval: Approval { id, .. },
            ..
        } = &pending
        else {
            unreachable!("built as an Approval");
        };
        fixture.0.pending_approvals.push(*id);
        fixture.activity(pending);

        let reading = fixture.window(Window {
            detail: Detail::Activities,
            ..Window::default()
        });
        assert_eq!(
            reading.approvals,
            [
                "Approval 1.3 awaits the user's Decision on whether the Agent may run `cargo \
              nextest run`. It gives as its reason \"The tests need the network.\". Only the \
              user can decide it, so tell them it is waiting on them."
            ]
        );
        assert!(
            reading.transcript.contains(
                "1.2 approval [accepted]: may the Agent run `cargo build`?\n1.3 approval \
                 [awaiting the user's Decision]: may the Agent run `cargo nextest run`?"
            ),
            "{}",
            reading.transcript
        );
    }

    #[test]
    fn a_subagents_session_reads_its_delegations_and_who_sent_them() {
        let parent = SessionId::new();
        let sibling = SessionId::new();
        let mut fixture = Fixture::subagent_of(parent);
        let turn = fixture.continuation(TurnStatus::Completed);
        fixture.message(
            turn,
            MessageRole::Delegation(Delegator {
                session_id: parent,
                name: None,
            }),
            "Look around.",
        );
        fixture.message(
            turn,
            MessageRole::Delegation(Delegator {
                session_id: sibling,
                name: Some("Planner".to_owned()),
            }),
            "And check the docs.",
        );
        fixture.agent(turn, "Done.");

        let reading = fixture.window(Window::default());
        assert_eq!(
            reading.transcript,
            format!(
                "[Turn 1 of 1 · completed at 2026-10-01T09:32:03Z after 2m 3s]\n\
                 1.1 delegation: Look around.\n\
                 1.2 delegation from Session {sibling}: And check the docs.\n\
                 1.3 agent: Done."
            ),
            "a Turn its spawn began is no Continuation"
        );
    }

    #[test]
    fn a_turn_nothing_asked_for_is_headed_a_continuation() {
        let mut fixture = Fixture::new();
        let turn = fixture.continuation(TurnStatus::Interrupted);
        fixture.agent(turn, "Woken by the Subagent's Report.");
        assert!(fixture.window(Window::default()).transcript.starts_with(
            "[Turn 1 of 1 · Continuation · interrupted at 2026-10-01T09:32:03Z after 2m 3s]"
        ));
    }

    #[test]
    fn a_subagent_that_waits_on_the_user_is_named() {
        let mut fixture = Fixture::new();
        let subagent = SessionId::new();
        fixture
            .0
            .subagent_interventions
            .push(SubagentInterventions {
                session_id: subagent,
                via_session_id: subagent,
                revision: SessionRevision(3),
                pending_questionnaires: vec![QuestionnaireId::new()],
                submitting_questionnaires: Vec::new(),
                pending_approvals: vec![ApprovalId::new(), ApprovalId::new()],
                submitting_approvals: Vec::new(),
            });
        assert_eq!(
            fixture.window(Window::default()).subagent_interventions,
            [format!(
                "A Subagent's Session beneath this one, {subagent}, holds 1 Questionnaire \
                 awaiting an Answer and 2 Approvals awaiting the user's Decision; read that \
                 Session for them."
            )]
        );
    }

    #[test]
    fn a_session_with_no_turns_reads_as_nothing_left_out() {
        let fixture = Fixture::new();
        let reading = fixture.window(Window::default());
        assert_eq!(reading.transcript, "");
        assert_eq!((reading.before, reading.earlier), (None, None));
    }

    #[test]
    fn a_point_or_entry_the_session_does_not_hold_is_refused() {
        let fixture = three_turns();
        let at = |before: &str| {
            fixture.refused(ReadRequest::Window(Window {
                before: Some(before.parse().expect("a point")),
                ..Window::default()
            }))
        };
        assert_eq!(at("4"), ReadRefusal::NoSuchTurn { turn: 4, turns: 3 });
        assert_eq!(
            at("3.9"),
            ReadRefusal::NoSuchEntry {
                entry: "3.9".parse().expect("a number"),
                entries: 7
            }
        );
        assert_eq!(
            fixture
                .window(Window {
                    before: before("2.1.99"),
                    ..Window::default()
                })
                .transcript,
            fixture
                .window(Window {
                    before: before("2.1.10"),
                    ..Window::default()
                })
                .transcript,
            "a point past an entry's end stands at its end"
        );
        assert!(
            fixture
                .read(ReadRequest::Window(Window {
                    before: before("3.8"),
                    ..Window::default()
                }))
                .transcript
                .ends_with("3.7 agent: Fixed the race in sync; the tests pass."),
            "the point past a Turn's last entry is that Turn's end"
        );
        assert_eq!(
            fixture.refused(entry("9.1")),
            ReadRefusal::NoSuchTurn { turn: 9, turns: 3 }
        );
    }

    #[test]
    fn a_point_is_spelled_as_briefly_as_it_can_be_and_read_back_as_spelled() {
        for (spelled, briefly) in [
            ("4", "4"),
            ("4.1", "4"),
            ("4.1.0", "4"),
            ("4.7", "4.7"),
            ("4.7.0", "4.7"),
            ("4.7.120", "4.7.120"),
            (" 12.3.4 ", "12.3.4"),
        ] {
            let position = spelled.parse::<Position>().expect("a point");
            assert_eq!(position.to_string(), briefly, "{spelled:?}");
        }
        for refused in ["", "0", "4.0", "four", "4.7.120.1", "-1", "4..7"] {
            assert!(refused.parse::<Position>().is_err(), "{refused:?}");
        }
        assert_eq!(
            "4.7"
                .parse::<EntryNumber>()
                .map(|number| number.to_string()),
            Ok("4.7".to_owned())
        );
        for refused in ["4", "4.0", "0.1", "4.7.1"] {
            assert!(refused.parse::<EntryNumber>().is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_message_a_sidekick_sent_names_the_sidekick_rather_than_the_user() {
        let sidekick = SessionId::new();
        let author = |title: &str| {
            Some(Author::Sidekick {
                session_id: sidekick,
                title: title.to_owned(),
            })
        };
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        fixture.authored(
            turn,
            MessageRole::User,
            author("Tidy the ledger"),
            "Pick the parser back up.",
        );
        fixture.user(turn, "And mind the tests.");
        fixture.authored(turn, MessageRole::User, author(""), "Steer it this way.");
        fixture.authored(
            turn,
            MessageRole::User,
            author(&"Long ".repeat(40)),
            "One more thing.",
        );
        fixture.agent(turn, "Done.");

        let long = format!("{}…", "Long ".repeat(12).trim_end());
        assert_eq!(long.chars().count(), TITLE_CHARS);
        let reading = fixture.window(Window::default());
        assert_eq!(
            reading.transcript,
            format!(
                "[Turn 1 of 1 · completed at 2026-10-01T09:32:03Z after 2m 3s]\n\
                 1.1 sent by Sidekick \"Tidy the ledger\" (Session {sidekick}): Pick the parser \
                 back up.\n\
                 1.2 user: And mind the tests.\n\
                 1.3 sent by a Sidekick (Session {sidekick}): Steer it this way.\n\
                 1.4 sent by Sidekick \"{long}\" (Session {sidekick}): One more thing.\n\
                 1.5 agent: Done."
            ),
            "the user's own words are the user's, and a Sidekick's say whose they are"
        );
        assert_eq!(
            fixture.read(entry("1.1")).transcript,
            format!(
                "1.1 sent by Sidekick \"Tidy the ledger\" (Session {sidekick}): Pick the parser \
                 back up."
            )
        );
    }

    /// However small the cap, a read shows something of the newest entry it
    /// reaches beneath its heading — however long that heading, and however
    /// long the Title of a Sidekick that sent the entry — so the read after
    /// it moves on: a Subagent's Delegation from another Subagent, and a
    /// top-level Session's Continuation holding a Message a Sidekick sent.
    #[test]
    fn the_smallest_cap_still_shows_something_beneath_the_longest_heading() {
        let sidekick = Author::Sidekick {
            session_id: SessionId::new(),
            title: "t".repeat(10 * TITLE_CHARS),
        };
        let delegated = MessageRole::Delegation(Delegator {
            session_id: SessionId::new(),
            name: None,
        });
        for (mut fixture, role, author) in [
            (Fixture::subagent_of(SessionId::new()), delegated, None),
            (Fixture::new(), MessageRole::User, Some(sidekick)),
        ] {
            for _ in 0..9_999 {
                fixture.turn(TurnStatus::Completed);
            }
            let turn = fixture.continuation(TurnStatus::Failed);
            for _ in 0..9_999 {
                fixture.command(turn, "true", "");
            }
            fixture.activity(Activity::Error {
                id: ActivityId::new(),
                turn_id: turn,
                text: "e".repeat(10 * LINE_CHARS),
            });
            fixture.0.turns.last_mut().expect("the Turn").settled_at =
                Some(SessionTimestamp(BEGAN + 99_999 * 3_600_000));
            for _ in 0..9_999 {
                fixture.message(turn, MessageRole::Agent, "aside");
            }
            fixture.authored(turn, role, author, &"d".repeat(DEFAULT_MAX_CHARS));
            let reading = fixture.window(Window {
                max_chars: 1,
                ..Window::default()
            });
            assert_eq!(said(&reading.transcript), 1, "{}", reading.transcript);
            assert!(
                reading.transcript.starts_with("[Turn 10000 of 10000 · ")
                    && reading.transcript.ends_with(": […]d"),
                "the heading, the entry's label and one character of it: {}",
                reading.transcript
            );
            assert_eq!(
                reading.before.expect("the cut is stated").to_string(),
                format!("10000.20000.{}", DEFAULT_MAX_CHARS - 1)
            );
        }
    }

    /// A point given within a Message reads on through the rest of that
    /// Message whatever has been written since: a read cut the Agent's final
    /// Message in a working Turn, and the Message written after it, final
    /// now, hides nothing of what was left of it.
    #[test]
    fn a_point_within_a_message_reads_the_rest_of_it_after_later_messages_arrive() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Active);
        fixture.user(turn, "Explain the ledger.");
        let explanation = "The ledger balances every account each night. ".repeat(20);
        fixture.agent(turn, &explanation);
        let first = fixture.window(Window {
            max_chars: 100,
            ..Window::default()
        });
        let point = first.before.expect("the Message was cut");
        let (_, kept) = first
            .transcript
            .split_once("\n1.2 agent: […]")
            .expect("the Message lost its start");

        fixture.agent(turn, "Now I am running the tests.");
        let rest = fixture.window(Window {
            before: Some(point),
            max_chars: usize::MAX,
            ..Window::default()
        });
        let (heading, head) = rest
            .transcript
            .split_once("\n1.1 user: Explain the ledger.\n1.2 agent: ")
            .expect("the rest of the Message, beneath what asked for it");
        assert_eq!(
            heading, "[Turn 1 of 1 · working since 2026-10-01T09:30:00Z]",
            "nothing read is said to be left out"
        );
        assert_eq!(format!("{head}{kept}"), explanation);
        assert_eq!((rest.before, rest.earlier), (None, None));
        assert!(
            fixture
                .window(Window::default())
                .transcript
                .ends_with("\n1.3 agent: Now I am running the tests."),
            "a fresh read shows the final Message as it is now"
        );
    }

    /// An entry the cap left out whole is read whole from the point given
    /// after it, at whatever detail the read on asks for — an Agent Message
    /// no longer its Turn's final one among them.
    #[test]
    fn an_entry_the_cap_left_out_whole_is_read_from_the_point_given_at_any_detail() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        fixture.user(turn, "Run the tests.");
        fixture.agent(turn, "Running them.");
        fixture.command(turn, "cargo test", "ok");
        fixture.agent(turn, "Done.");

        let reading = fixture.window(Window {
            max_chars: 15,
            detail: Detail::Activities,
            ..Window::default()
        });
        assert_eq!(
            reading.transcript,
            "[Turn 1 of 1 · completed at 2026-10-01T09:32:03Z after 2m 3s]\n\
             1.3 command [completed]: cargo test\n\
             1.4 agent: Done."
        );
        assert_eq!(reading.before, before("1.2.13"));
        let rest = fixture.window(Window {
            before: reading.before,
            ..Window::default()
        });
        assert_eq!(
            rest.transcript,
            "[Turn 1 of 1 · completed at 2026-10-01T09:32:03Z after 2m 3s]\n\
             1.1 user: Run the tests.\n\
             1.2 agent: Running them."
        );
    }

    /// In a Subagent's Session a Turn its spawn began is no Continuation, nor
    /// is one a Delegation began, but one nothing asked for — its Watch
    /// waking it — is.
    #[test]
    fn a_subagents_turn_nothing_asked_for_is_headed_a_continuation() {
        let parent = SessionId::new();
        let mut fixture = Fixture::subagent_of(parent);
        let spawned = fixture.continuation(TurnStatus::Completed);
        fixture.agent(spawned, "Looked around.");
        let resumed = fixture.continuation(TurnStatus::Completed);
        fixture.message(
            resumed,
            MessageRole::Delegation(Delegator {
                session_id: parent,
                name: None,
            }),
            "Look again.",
        );
        fixture.agent(resumed, "Looked again.");
        let woken = fixture.continuation(TurnStatus::Completed);
        fixture.activity(Activity::WatchOutcome {
            id: ActivityId::new(),
            turn_id: woken,
            status: WatchOutcomeStatus::Completed,
            description: "cargo test".to_owned(),
            summary: None,
        });
        fixture.agent(woken, "The tests finished.");

        let headings = fixture
            .window(Window {
                turns: 3,
                ..Window::default()
            })
            .transcript
            .lines()
            .filter(|line| line.starts_with("[Turn "))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            headings,
            [
                "[Turn 1 of 3 · completed at 2026-10-01T09:32:03Z after 2m 3s]",
                "[Turn 2 of 3 · completed at 2026-10-01T09:32:03Z after 2m 3s]",
                "[Turn 3 of 3 · Continuation · completed at 2026-10-01T09:32:03Z after 2m 3s \
                 · 1 Activity not shown]",
            ]
        );
    }

    /// A Questionnaire read whole says of each Question only the Answers it
    /// accepts: a choice only where it offers choices, free text only where
    /// it takes it, choices and free text together only where it takes both
    /// at once, and leaving it unanswered only where it need not be answered.
    #[test]
    fn a_questionnaire_read_whole_says_only_the_answers_each_question_accepts() {
        let choice = |id: &str| QuestionChoice {
            id: id.to_owned(),
            label: id.to_uppercase(),
            description: None,
            recommended: false,
        };
        let question = |id: &str, choices: Vec<QuestionChoice>| Question {
            id: id.to_owned(),
            title: None,
            text: format!("Which {id}?"),
            choices,
            multiple: false,
            freeform: false,
            combine_freeform: false,
            secret: false,
            required: true,
        };
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Active);
        fixture.activity(Activity::Questionnaire {
            id: ActivityId::new(),
            turn_id: turn,
            questionnaire: Questionnaire {
                id: QuestionnaireId::new(),
                questions: vec![
                    question("colour", vec![choice("red"), choice("blue")]),
                    Question {
                        multiple: true,
                        freeform: true,
                        combine_freeform: true,
                        required: false,
                        ..question("tags", vec![choice("fast"), choice("safe")])
                    },
                    Question {
                        freeform: true,
                        secret: true,
                        ..question("password", Vec::new())
                    },
                    Question {
                        freeform: true,
                        ..question("reason", vec![choice("other")])
                    },
                ],
            },
            outcome: QuestionnaireOutcome::Pending,
            answer: None,
        });

        assert_eq!(
            fixture.read(entry("1.1")).transcript,
            "1.1 questionnaire [awaiting an Answer]:\n\
             Question 1 (colour): Which colour?\n  takes one of the choices; required\n  \
             - red: RED\n  - blue: BLUE\n\
             Question 2 (tags): Which tags?\n  takes one or more of the choices, free text or \
             one or more of the choices with free text beside them; may be left unanswered\n  \
             - fast: FAST\n  - safe: SAFE\n\
             Question 3 (password): Which password?\n  takes free text; required; its Answer is \
             secret\n\
             Question 4 (reason): Which reason?\n  takes one of the choices or free text; \
             required\n  - other: OTHER"
        );
    }

    #[test]
    fn a_file_change_and_a_subagent_read_whole_say_all_suru_holds_of_them() {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        let root = PathBuf::from(ROOT);
        fixture.activity(Activity::FileChange {
            id: ActivityId::new(),
            turn_id: turn,
            status: ActivityStatus::Completed,
            changes: vec![
                FileChange::Add {
                    path: root.join("new.rs"),
                },
                FileChange::Delete {
                    path: root.join("old.rs"),
                },
                FileChange::Update {
                    path: root.join("lib.rs"),
                    moved_to: None,
                },
                FileChange::Update {
                    path: root.join("a.rs"),
                    moved_to: Some(root.join("b.rs")),
                },
            ],
        });
        let subagent = SessionId::new();
        fixture.activity(Activity::Subagent {
            id: ActivityId::new(),
            turn_id: turn,
            status: ActivityStatus::Failed,
            name: "Scout".to_owned(),
            description: "Check the other tests".to_owned(),
            model: Some(ModelId::new("gpt-5.5")),
            session_id: subagent,
            brokered: true,
            duration_ms: Some(65_000),
        });

        let at = |file: &str| root.join(file).display().to_string();
        assert_eq!(
            fixture.read(entry("1.1")).transcript,
            format!(
                "1.1 file change [completed]:\nadd {}\ndelete {}\nupdate {}\nmove {} to {}",
                at("new.rs"),
                at("old.rs"),
                at("lib.rs"),
                at("a.rs"),
                at("b.rs"),
            ),
            "read whole, a File Change gives each path in full"
        );
        assert_eq!(
            fixture.read(entry("1.2")).transcript,
            format!(
                "1.2 subagent [failed, Session {subagent}]: Scout — Check the other tests\n\
                 model: gpt-5.5\nworked 1m 5s\nIts work is in its own Session, {subagent}; read \
                 that Session for it."
            )
        );
        assert!(
            fixture
                .window(Window {
                    detail: Detail::Activities,
                    ..Window::default()
                })
                .transcript
                .contains(
                    "\n1.1 file change [completed]: add new.rs, delete old.rs, update lib.rs, move \
                     a.rs to b.rs\n"
                ),
            "its line says each path from the Session's own directory"
        );
    }

    /// A Sidekick's Session whose one Turn began the Subsession `subsession`.
    fn began_a_subsession(subsession: SessionId) -> Fixture {
        let mut fixture = Fixture::new();
        let turn = fixture.turn(TurnStatus::Completed);
        fixture.user(turn, "Get the login test fixed.");
        fixture.activity(Activity::Subsession {
            id: ActivityId::new(),
            turn_id: turn,
            session_id: subsession,
            title: "Flaky login test".to_owned(),
            prompt: "Fix the flaky login test in the auth suite.\nIt fails one run in ten."
                .to_owned(),
        });
        fixture.agent(turn, "Begun; I will check on it.");
        fixture
    }

    #[test]
    fn a_subsession_a_sidekick_began_is_a_line_naming_its_session_title_and_first_prompt() {
        let subsession = SessionId::from_uuid(uuid::Uuid::nil());
        let fixture = began_a_subsession(subsession);

        assert_eq!(
            fixture
                .window(Window {
                    detail: Detail::Activities,
                    ..Window::default()
                })
                .transcript,
            format!(
                "[Turn 1 of 1 · completed at 2026-10-01T09:32:03Z after 2m 3s]\n\
                 1.1 user: Get the login test fixed.\n\
                 1.2 subsession [Session {subsession}]: \"Flaky login test\", first asked: Fix \
                 the flaky login test in the auth suite.…\n\
                 1.3 agent: Begun; I will check on it."
            ),
            "the activities detail names the Subsession's Session, Title and first Prompt, on \
             one line as every Activity's"
        );
        assert!(
            !fixture
                .window(Window::default())
                .transcript
                .contains("subsession"),
            "and the messages detail leaves it out, as every Activity"
        );
        assert_eq!(
            fixture.read(entry("1.2")).transcript,
            format!(
                "1.2 subsession [Session {subsession}]: Flaky login test\n\
                 first asked: Fix the flaky login test in the auth suite.\n\
                 It fails one run in ten.\n\
                 It works in its own Session, {subsession}; read that Session for it."
            ),
            "read whole, it holds the whole first Prompt and says where to follow it"
        );
    }
}
