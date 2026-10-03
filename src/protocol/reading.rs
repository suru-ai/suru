//! Reading a Session for an Agent, as Servers say it to each other: what a
//! read asks for, and the excerpt the Server holding the Session answers it
//! with (ADR 0049). The Server holding the Session windows it — numbering
//! its entries, capping what they say, giving the point to read on from — so
//! only what was asked for crosses a Pairing, and the reader names the other
//! Servers that excerpt refers to as it reaches them. How a reading numbers
//! a Session's entries is part of what Servers say to each other: a point one
//! Server gives, another passes back to it, so changing how entries are
//! numbered changes the protocol version.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{Author, Questionnaire, SessionId, SessionSummary};

/// How many Turns a reading holds unless asked for another number.
pub const DEFAULT_TURNS: usize = 1;

/// How many characters of what its entries say a reading shows at most
/// unless asked for another number.
pub const DEFAULT_MAX_CHARS: usize = 2_000;

/// How much a reading shows of each Turn it holds.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    /// The user Messages and Delegations that asked for each Turn's work, and
    /// the Agent's final Message in it.
    #[default]
    Messages,
    /// Every Message, and one line for each Activity, without its output.
    Activities,
}

impl Detail {
    pub const NAMES: [&'static str; 2] = ["messages", "activities"];

    pub fn named(name: &str) -> Option<Self> {
        match name {
            "messages" => Some(Self::Messages),
            "activities" => Some(Self::Activities),
            _ => None,
        }
    }

    /// The name it is asked for by.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::Activities => "activities",
        }
    }
}

/// One Message or Activity as a reading numbers it: its Turn, counted from
/// the Session's first, and its place among that Turn's entries, each from 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryNumber {
    pub(crate) turn: usize,
    pub(crate) entry: usize,
}

impl EntryNumber {
    /// The number of the Turn the entry stands in.
    pub const fn turn(self) -> usize {
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
pub struct Position {
    pub(crate) turn: usize,
    pub(crate) entry: usize,
    pub(crate) chars: usize,
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

/// Entry numbers and points travel spelled as a reading spells them.
macro_rules! spelled_on_the_wire {
    ($type:ty, $what:literal) => {
        impl Serialize for $type {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let spelled = String::deserialize(deserializer)?;
                spelled
                    .parse()
                    .map_err(|()| serde::de::Error::custom(format!("{spelled:?} is not {}", $what)))
            }
        }
    };
}

spelled_on_the_wire!(EntryNumber, "an entry's number");
spelled_on_the_wire!(Position, "a point in a Transcript");

/// What a read asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadRequest {
    /// The latest Turns, or those before a point, capped from their end.
    Window(Window),
    /// One Message or Activity, whole.
    Entry(EntryNumber),
}

/// Which Turns a read holds, how much of them, and how much it shows of each.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Window {
    pub turns: usize,
    pub max_chars: usize,
    pub before: Option<Position>,
    pub detail: Detail,
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadRefusal {
    /// A Turn the Session has not reached; it has `turns`.
    NoSuchTurn { turn: usize, turns: usize },
    /// An entry its Turn does not hold; that Turn holds `entries`.
    NoSuchEntry { entry: EntryNumber, entries: usize },
}

/// A reading of a Session as the Server holding it gives it: everything the
/// reading holds, its words already chosen, but for the other Servers it
/// names, which stand as [`ServerReference`]s for the reader to name as it
/// reaches them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExcerpt {
    /// Every Questionnaire waiting on an Answer, in Transcript order.
    pub questionnaires: Vec<ExcerptQuestionnaire>,
    /// Each Approval waiting on the user's Decision, said as a sentence.
    pub approvals: Vec<String>,
    /// Each Subagent's Session beneath this one that waits on the user, said
    /// as a sentence.
    pub subagent_interventions: Vec<String>,
    /// The Turns read, or the entry read whole.
    pub transcript: Vec<ReadingSpan>,
    /// The point to read back from for whatever was left out before the
    /// transcript, and `None` when it reaches the Session's beginning.
    pub before: Option<Position>,
    /// What was left out before the transcript, said as a sentence naming
    /// `before`; `None` exactly when `before` is.
    pub earlier: Option<String>,
    /// The Sidekick that began the Session, where it is a Subsession.
    pub begun_by: Option<Author>,
}

/// A Questionnaire waiting on an Answer, and its entry's number.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExcerptQuestionnaire {
    pub entry: EntryNumber,
    pub questionnaire: Questionnaire,
}

/// A stretch of an excerpt's transcript: words as they stand, or another
/// Server's naming, left to the reader.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingSpan {
    Text(String),
    Names(ServerReference),
}

/// What a reading names that only its reader can say: who acted on the
/// user's behalf, or where a Session on another Server lives — each named by
/// the Server holding the Session read, as its own Pairings know it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerReference {
    /// Who sent a Message, answered a Questionnaire, or began the Session.
    Actor(Author),
    /// A Subsession's line, naming its Session and where it lives.
    Subsession {
        session_id: SessionId,
        origin: Option<String>,
        origin_fingerprint: Option<String>,
    },
    /// Where a Subsession read whole lives, and how to read it there.
    SubsessionReach {
        origin: Option<String>,
        origin_fingerprint: Option<String>,
    },
}

/// What a Server's `GET /v1/sessions/{session_id}/reading` answers that names
/// what its Session holds: the Session's summary, taken in the same moment as
/// the excerpt, so how it stands and what it says agree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionReadingAnswer {
    pub summary: SessionSummary,
    pub reading: Result<SessionExcerpt, ReadRefusal>,
}
