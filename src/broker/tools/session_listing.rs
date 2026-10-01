//! `list_sessions`: a Sidekick's listing of the Sessions on its own Server, as
//! compact rows that cost it few tokens.
//!
//! It lists what the user's own listing lists — top-level Sessions, never a
//! Subagent's — and reads each as that listing does: its Standing by the same
//! precedence, and whether it is settled by the user's say-so or by the same
//! auto-settle Setting, read against the Server's clock, so "active" means
//! what the user sees. By default it answers the ordinary question cheaply:
//! active Sessions, most recently active first, twenty of them, and how many
//! more there were.

use std::cmp::Reverse;

use serde::Serialize;
use serde_json::{Map, Value, json};
use time::{
    Date, OffsetDateTime, format_description::well_known::Rfc3339, macros::format_description,
};

use super::{ToolRefusal, listed};
use crate::protocol::{
    AutoSettle, SessionId, SessionListItem, SessionStanding, SessionTimestamp, StandingReading,
};

/// How many rows a listing answers with unless asked for another number.
const DEFAULT_LIMIT: u64 = 20;
/// The fewest and most rows a listing may be asked for; a limit outside them
/// is kept at the nearest, as a wait's timeout is.
const MIN_LIMIT: u64 = 1;
const MAX_LIMIT: u64 = 100;

pub(super) const DESCRIPTION: &str = "\
List the Sessions on this Suru server — the user's work in every Workspace — \
as compact rows, most recently active first. Every argument is optional: \
\"workspace\", the path of a Workspace as rows give it, to list only the \
Sessions working there; \"title\", words a Session's Title contains, matched \
whatever their case; \"liveness\": \"active\", the default, for the Sessions \
the user has not set aside, \"settled\" for those set aside — by the user, or \
on their own after long enough idle, as the user's own listing sets them \
aside — or \"all\"; \"standing\", one of \"needs_intervention\", \"working\", \
\"failed\", \"monitoring\", \"done\" or \"none\", to list only Sessions \
standing so; \"active_after\" and \"active_before\", each an RFC 3339 moment \
such as 2026-10-01T09:30:00Z or a day such as 2026-10-01, which stands for \
its first moment in UTC, to list only Sessions last active at or after the \
one and before the other; and \"limit\", how many rows at most: 20 unless \
given, and never fewer than 1 nor more than 100, a limit outside those kept \
at the nearest. Answers with JSON of the shape {\"sessions\": [row, ...], \
\"omitted\": n}, where \"omitted\" counts the Sessions that matched but were \
left out past the limit; ask again with a narrower filter or a higher limit \
for them. Each row has \"session_id\"; \"title\"; \"workspace\", the path of \
the Workspace it works in; \"standing\", what it says of its work: \
\"needs_intervention\" while a Questionnaire or Approval in it waits on the \
user, \"working\", \"failed\" or \"done\" when its latest Turn ended so and no \
one has looked at it since, \"monitoring\" while something it left running \
may wake it, or null; \"last_active\", the RFC 3339 moment it last moved; and \
\"settled\", whether it is set aside. A row for a Session Suru could not read \
also carries \"unreadable\": true. A Subagent's Session is never listed.";

/// The JSON Schema of `list_sessions`' arguments. Bounds the description
/// states — the limit's — are left to it: a limit outside them is kept at the
/// nearest, never refused, which a schema bound would have a harness do.
pub(super) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "workspace": {
                "type": "string",
                "description": "The path of a Workspace, as rows give it, to list only the \
                    Sessions working there.",
            },
            "title": {
                "type": "string",
                "description": "Words a Session's Title contains, matched whatever their case.",
            },
            "liveness": {
                "type": "string",
                "enum": Liveness::NAMES,
                "description": "\"active\" (the default), \"settled\", or \"all\".",
            },
            "standing": {
                "type": "string",
                "enum": StandingFilter::NAMES,
                "description": "List only Sessions standing so; \"none\" for those whose \
                    Standing says nothing.",
            },
            "active_after": {
                "type": "string",
                "description": "An RFC 3339 moment or a YYYY-MM-DD day: list only Sessions \
                    last active at or after it.",
            },
            "active_before": {
                "type": "string",
                "description": "An RFC 3339 moment or a YYYY-MM-DD day: list only Sessions \
                    last active before it.",
            },
            "limit": {
                "type": "integer",
                "description": "How many rows at most: 20 unless given, from 1 to 100.",
            },
        },
        "additionalProperties": false,
    })
}

/// What `list_sessions` was called with, each argument checked for the shape
/// its schema gives it.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ListArguments {
    workspace: Option<String>,
    title: Option<String>,
    liveness: Liveness,
    standing: Option<StandingFilter>,
    active_after: Option<SessionTimestamp>,
    active_before: Option<SessionTimestamp>,
    limit: usize,
}

impl Default for ListArguments {
    fn default() -> Self {
        Self {
            workspace: None,
            title: None,
            liveness: Liveness::Active,
            standing: None,
            active_after: None,
            active_before: None,
            limit: DEFAULT_LIMIT as usize,
        }
    }
}

impl ListArguments {
    const TAKES: [&'static str; 7] = [
        "workspace",
        "title",
        "liveness",
        "standing",
        "active_after",
        "active_before",
        "limit",
    ];

    pub(super) fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        if let Some(unknown) = arguments
            .keys()
            .find(|argument| !Self::TAKES.contains(&argument.as_str()))
        {
            return Err(ToolRefusal::new(format!(
                "list_sessions takes no argument `{unknown}`; it takes {}.",
                listed(Self::TAKES.into_iter())
            )));
        }
        let text = |argument: &str| match arguments.get(argument) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => {
                let text = text.trim();
                Ok((!text.is_empty()).then(|| text.to_owned()))
            }
            Some(_) => Err(ToolRefusal::new(format!(
                "list_sessions' `{argument}` must be a string."
            ))),
        };
        let liveness = match text("liveness")? {
            None => Liveness::Active,
            Some(named) => Liveness::named(&named).ok_or_else(|| {
                ToolRefusal::new(format!(
                    "list_sessions' `liveness` must be `active`, `settled` or `all`; `{named}` is \
                     none of them."
                ))
            })?,
        };
        let standing = match text("standing")? {
            None => None,
            Some(named) => Some(StandingFilter::named(&named).ok_or_else(|| {
                ToolRefusal::new(format!(
                    "list_sessions' `standing` must be one of {}; `{named}` is none of them.",
                    listed(StandingFilter::NAMES.into_iter())
                ))
            })?),
        };
        let moment = |argument: &str| {
            text(argument)?
                .map(|spelled| {
                    moment(&spelled).ok_or_else(|| {
                        ToolRefusal::new(format!(
                            "list_sessions' `{argument}` must be an RFC 3339 moment, such as \
                             2026-10-01T09:30:00Z, or a day, such as 2026-10-01; `{spelled}` is \
                             neither."
                        ))
                    })
                })
                .transpose()
        };
        let limit = match arguments.get("limit") {
            None | Some(Value::Null) => DEFAULT_LIMIT,
            Some(Value::Number(number)) if number.is_u64() => number
                .as_u64()
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(MIN_LIMIT, MAX_LIMIT),
            // A whole number below one, written as a negative, is kept at the
            // nearest as one above the ceiling is.
            Some(Value::Number(number)) if number.is_i64() => MIN_LIMIT,
            Some(_) => {
                return Err(ToolRefusal::new(format!(
                    "list_sessions' `limit` must be a whole number of rows, from {MIN_LIMIT} to \
                     {MAX_LIMIT}."
                )));
            }
        };
        Ok(Self {
            workspace: text("workspace")?,
            title: text("title")?,
            liveness,
            standing,
            active_after: moment("active_after")?,
            active_before: moment("active_before")?,
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        })
    }

    /// Whether `session`, settled or not as `settled` says, is one this
    /// listing asks for.
    fn admits(&self, session: &SessionListItem, settled: bool) -> bool {
        let liveness = match self.liveness {
            Liveness::Active => !settled,
            Liveness::Settled => settled,
            Liveness::All => true,
        };
        let workspace = self.workspace.as_deref().is_none_or(|named| {
            session.workspace().is_some_and(|workspace| {
                workspace.id.0 == named || workspace.path == std::path::Path::new(named)
            })
        });
        // Matched as the Sidebar's search matches a Title: a plain
        // case-insensitive substring, the words a reader remembers.
        let title = self.title.as_deref().is_none_or(|words| {
            session
                .title()
                .to_lowercase()
                .contains(&words.to_lowercase())
        });
        let standing = self
            .standing
            .is_none_or(|filter| filter.admits(session.standing()));
        let last_active = session.updated_at();
        let after = self.active_after.is_none_or(|after| last_active >= after);
        let before = self.active_before.is_none_or(|before| last_active < before);
        liveness && workspace && title && standing && after && before
    }
}

/// Which Sessions a listing holds by whether they are set aside.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Liveness {
    Active,
    Settled,
    All,
}

impl Liveness {
    const NAMES: [&'static str; 3] = ["active", "settled", "all"];

    fn named(name: &str) -> Option<Self> {
        match name {
            "active" => Some(Self::Active),
            "settled" => Some(Self::Settled),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// The one Standing a listing is narrowed to, or the absence of any.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StandingFilter {
    Standing(SessionStanding),
    None,
}

impl StandingFilter {
    const NAMES: [&'static str; 6] = [
        "needs_intervention",
        "working",
        "failed",
        "monitoring",
        "done",
        "none",
    ];

    fn named(name: &str) -> Option<Self> {
        if name == "none" {
            return Some(Self::None);
        }
        STANDINGS
            .into_iter()
            .find(|standing| standing_name(*standing) == name)
            .map(Self::Standing)
    }

    fn admits(self, standing: Option<SessionStanding>) -> bool {
        match self {
            Self::Standing(wanted) => standing == Some(wanted),
            Self::None => standing.is_none(),
        }
    }
}

/// Every Standing, in its order of precedence.
const STANDINGS: [SessionStanding; 5] = [
    SessionStanding::NeedsIntervention,
    SessionStanding::Working,
    SessionStanding::Failed,
    SessionStanding::Monitoring,
    SessionStanding::Done,
];

/// A Standing as a row and the `standing` argument spell it.
const fn standing_name(standing: SessionStanding) -> &'static str {
    match standing {
        SessionStanding::NeedsIntervention => "needs_intervention",
        SessionStanding::Working => "working",
        SessionStanding::Failed => "failed",
        SessionStanding::Monitoring => "monitoring",
        SessionStanding::Done => "done",
    }
}

/// The moment `spelled` names: an RFC 3339 moment, or a day, which stands for
/// its first moment in UTC. A moment before the epoch is the epoch, which no
/// Session predates.
fn moment(spelled: &str) -> Option<SessionTimestamp> {
    let moment = OffsetDateTime::parse(spelled, &Rfc3339)
        .or_else(|_| {
            Date::parse(spelled, format_description!("[year]-[month]-[day]"))
                .map(|day| day.midnight().assume_utc())
        })
        .ok()?;
    let millis = moment.unix_timestamp_nanos() / 1_000_000;
    Some(SessionTimestamp(u64::try_from(millis).unwrap_or(0)))
}

/// `at` as a row spells it: an RFC 3339 moment in UTC, to the millisecond a
/// Session's timestamps are kept to, so naming it back as a bound names it
/// exactly.
fn spelled(at: SessionTimestamp) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at.0) * 1_000_000)
        .ok()
        .and_then(|moment| moment.format(&Rfc3339).ok())
        .unwrap_or_else(|| at.0.to_string())
}

/// What `list_sessions` answers.
#[derive(Debug, Serialize)]
pub(super) struct SessionListing {
    sessions: Vec<ListedSession>,
    /// How many Sessions the listing matched but left out past its limit.
    omitted: usize,
}

/// One row of a listing.
#[derive(Debug, Serialize)]
struct ListedSession {
    session_id: SessionId,
    title: String,
    /// The path of the Workspace the Session works in — its presented root —
    /// and `None` for a Session Suru could not read that no longer says.
    workspace: Option<String>,
    standing: Option<&'static str>,
    last_active: String,
    settled: bool,
    /// Set only for a Session Suru could not read, which nothing but a
    /// listing can do anything with.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    unreadable: bool,
}

/// The listing `arguments` ask for, of `sessions` — every top-level Session on
/// the Server — settled as `auto_settle` settles them as of `now`.
pub(super) fn listing(
    mut sessions: Vec<SessionListItem>,
    arguments: &ListArguments,
    auto_settle: AutoSettle,
    now: SessionTimestamp,
) -> SessionListing {
    sessions.sort_by_key(|session| Reverse(session.updated_at()));
    let mut matched = sessions
        .iter()
        .filter_map(|session| {
            let settled = auto_settle.settles(session, now);
            arguments
                .admits(session, settled)
                .then(|| listed_session(session, settled))
        })
        .collect::<Vec<_>>();
    let omitted = matched.len().saturating_sub(arguments.limit);
    matched.truncate(arguments.limit);
    SessionListing {
        sessions: matched,
        omitted,
    }
}

fn listed_session(session: &SessionListItem, settled: bool) -> ListedSession {
    ListedSession {
        session_id: session.id(),
        title: session.title().to_owned(),
        workspace: session
            .workspace()
            .map(|workspace| workspace.path.to_string_lossy().into_owned()),
        standing: StandingReading::of(session).standing().map(standing_name),
        last_active: spelled(session.updated_at()),
        settled,
        unreadable: session.readable().is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(arguments: Value) -> Result<ListArguments, ToolRefusal> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        ListArguments::read(&arguments)
    }

    #[test]
    fn no_arguments_ask_for_twenty_active_sessions() {
        assert_eq!(arguments(json!({})), Ok(ListArguments::default()));
        assert_eq!(
            ListArguments::default().limit,
            20,
            "the description promises twenty"
        );
    }

    #[test]
    fn a_limit_outside_its_bounds_is_kept_at_the_nearest() {
        for (asked, kept) in [
            (json!(0), 1),
            (json!(-4), 1),
            (json!(7), 7),
            (json!(5000), 100),
        ] {
            assert_eq!(
                arguments(json!({ "limit": asked })).map(|read| read.limit),
                Ok(kept),
                "{asked}"
            );
        }
        assert!(arguments(json!({ "limit": 2.5 })).is_err());
    }

    #[test]
    fn a_moment_is_an_rfc_3339_moment_or_the_first_moment_of_a_day() {
        assert_eq!(
            moment("1970-01-02"),
            Some(SessionTimestamp(24 * 60 * 60 * 1_000))
        );
        assert_eq!(
            moment("1970-01-01T00:00:01.250+00:00"),
            Some(SessionTimestamp(1_250))
        );
        assert_eq!(
            moment("1970-01-01T01:00:00+01:00"),
            Some(SessionTimestamp(0)),
            "an offset is honored"
        );
        assert_eq!(moment("yesterday"), None);
        assert_eq!(moment("1969-12-31"), Some(SessionTimestamp(0)));
    }

    #[test]
    fn a_row_spells_its_moment_so_naming_it_back_names_it_exactly() {
        for at in [0, 1_250, 1_790_000_000_123] {
            let at = SessionTimestamp(at);
            assert_eq!(moment(&spelled(at)), Some(at), "{}", spelled(at));
        }
        assert_eq!(spelled(SessionTimestamp(1_250)), "1970-01-01T00:00:01.25Z");
    }

    #[test]
    fn the_description_names_every_standing_and_liveness_a_listing_takes() {
        for name in StandingFilter::NAMES.into_iter().chain(Liveness::NAMES) {
            assert!(
                DESCRIPTION.contains(&format!("\"{name}\"")),
                "list_sessions' description names {name}"
            );
        }
        for standing in STANDINGS {
            assert_eq!(
                StandingFilter::named(standing_name(standing)),
                Some(StandingFilter::Standing(standing))
            );
        }
    }

    #[test]
    fn the_schema_takes_what_the_arguments_read() {
        let schema = input_schema();
        let mut properties = schema["properties"]
            .as_object()
            .expect("the schema names its properties")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        properties.sort_unstable();
        let mut takes = ListArguments::TAKES.to_vec();
        takes.sort_unstable();
        assert_eq!(properties, takes);
        assert_eq!(
            schema["required"],
            Value::Null,
            "every argument is optional"
        );
    }
}
