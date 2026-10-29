//! The Activity rows an Insights read tallies, read straight from storage
//! without reading any Session's history in (ADR 0056): the one query, and
//! each row decoded into the few typed fields a tally reads. What a row
//! counts toward is the Insights read's to say, not storage's. The Errands
//! it counts are read beside them, in the same pass.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

use diesel::{
    prelude::*,
    sql_types::{BigInt, Nullable, Text},
};

use crate::{
    errands::ErrandRecord,
    protocol::{
        ActivityStatus, ApprovalOutcome, CompactionTrigger, Decision, SessionId, SessionTimestamp,
        TurnId,
    },
};

use super::{
    StorageError, StorageRepository, activities, connect, errand_table::errands_began_within,
    on_blocking_task,
};

/// How many Sessions one query of [`StorageRepository::insights_reading`]
/// binds, well within the fewest host parameters any SQLite build allows.
const SESSIONS_PER_ACTIVITY_READ: usize = 500;

/// A field of an Activity's payload as storage spells it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Spelled<T> {
    /// The payload has none.
    Absent,
    /// A spelling this Suru reads.
    Known(T),
    /// A spelling this Suru does not read, as written.
    Unknown(String),
}

impl<T: serde::de::DeserializeOwned> Spelled<T> {
    /// The `T` that `spelling`, read out of a payload, spells as the
    /// payload spells it, so a tally matches every variant the protocol
    /// knows and a variant added there is read here too.
    pub(super) fn of(spelling: Option<String>) -> Self {
        use serde::de::{IntoDeserializer, value::StrDeserializer};
        let Some(spelling) = spelling else {
            return Self::Absent;
        };
        let deserializer: StrDeserializer<'_, serde::de::value::Error> =
            spelling.as_str().into_deserializer();
        match T::deserialize(deserializer) {
            Ok(known) => Self::Known(known),
            Err(_) => Self::Unknown(spelling),
        }
    }
}

/// What an Insights tally reads of one stored Activity, by its kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActivityTallyRow {
    Command {
        status: Spelled<ActivityStatus>,
    },
    FileChange {
        status: Spelled<ActivityStatus>,
    },
    /// A Tool Call, by its Tool's name where it records one, and the MCP
    /// server hosting it.
    ToolCall {
        name: Option<String>,
        server: Option<String>,
        status: Spelled<ActivityStatus>,
    },
    /// A delegation to a Subagent: a spawn or a resume.
    Subagent {
        status: Spelled<ActivityStatus>,
    },
    /// A Compaction, with its Context Fill before and after where known.
    Compaction {
        trigger: Spelled<CompactionTrigger>,
        status: Spelled<ActivityStatus>,
        before_tokens: Option<i64>,
        after_tokens: Option<i64>,
    },
    /// An Approval, with the Decision recorded for it.
    Approval {
        outcome: Spelled<ApprovalOutcome>,
        decision: Spelled<Decision>,
    },
}

/// Where [`StorageRepository::insights_reading`] streams the rows it reads.
pub(crate) trait ActivityRowSink: Send + 'static {
    fn add(&mut self, row: ActivityTallyRow);
}

/// The fields the query selects of an Activity row, each payload field
/// absent where its kind has none. Read by position, so the fields stand in
/// the order the query selects them.
#[derive(Queryable)]
struct SelectedFields {
    session_id: String,
    turn_id: String,
    kind: String,
    status: Option<String>,
    name: Option<String>,
    server: Option<String>,
    trigger: Option<String>,
    before_tokens: Option<i64>,
    after_tokens: Option<i64>,
    outcome: Option<String>,
    decision: Option<String>,
}

impl SelectedFields {
    /// The row a tally reads, none where its kind is no kind a tally reads.
    fn into_row(self) -> Option<ActivityTallyRow> {
        let status = Spelled::of(self.status);
        Some(match self.kind.as_str() {
            "command" => ActivityTallyRow::Command { status },
            "file_change" => ActivityTallyRow::FileChange { status },
            "tool_call" => ActivityTallyRow::ToolCall {
                name: self.name,
                server: self.server,
                status,
            },
            "subagent" => ActivityTallyRow::Subagent { status },
            "compaction" => ActivityTallyRow::Compaction {
                trigger: Spelled::of(self.trigger),
                status,
                before_tokens: self.before_tokens,
                after_tokens: self.after_tokens,
            },
            "approval" => ActivityTallyRow::Approval {
                outcome: Spelled::of(self.outcome),
                decision: Spelled::of(self.decision),
            },
            _ => return None,
        })
    }
}

impl StorageRepository {
    /// What an Insights read asks of storage, read on one blocking task over
    /// one connection: the Activity rows of the Turns `turns` names, streamed
    /// into `sink`, and the Errands recorded as beginning within
    /// `errands_began`.
    ///
    /// Of every Activity storage holds of those Turns of each Session, the
    /// sink is given what an Insights tally reads — its Tools, Compactions,
    /// and Approvals — read without reading any Session's history in (ADR
    /// 0056). Each row gives up only its kind, the Turn owning it, and the
    /// few fields of its payload a tally reads; no payload is deserialized.
    /// Storage lags a Session open in memory by whatever it has not saved
    /// yet, which the read accepts.
    ///
    /// The rows are read a few Sessions at a time, so no query binds more
    /// than [`SESSIONS_PER_ACTIVITY_READ`] of them, and streamed rather than
    /// held: a Session's rows are read whole, whichever of its Turns they
    /// belong to, and those of a Turn not named left out here. The Errands
    /// are read as [`StorageRepository::errand_reading`] reads them (ADR
    /// 0057).
    pub(crate) async fn insights_reading<S: ActivityRowSink>(
        &self,
        turns: HashMap<SessionId, HashSet<TurnId>>,
        sink: S,
        errands_began: Range<SessionTimestamp>,
    ) -> Result<InsightsReading<S>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("reading what Insights count", move || {
            let mut connection = connect(&path)?;
            let activities = stream_activity_rows(&mut connection, turns, sink)?;
            let errands = errands_began_within(&mut connection, errands_began)?;
            Ok(InsightsReading {
                activities,
                errands,
            })
        })
        .await
    }
}

/// What [`StorageRepository::insights_reading`] read: the sink the Activity
/// rows streamed into, and the Errands, earliest first.
pub(crate) struct InsightsReading<S> {
    pub(crate) activities: S,
    pub(crate) errands: Vec<ErrandRecord>,
}

/// Streams into `sink` the Activity rows of the Turns `turns` names, read
/// over `connection`, and answers the sink.
fn stream_activity_rows<S: ActivityRowSink>(
    connection: &mut SqliteConnection,
    turns: HashMap<SessionId, HashSet<TurnId>>,
    mut sink: S,
) -> Result<S, StorageError> {
    let turns = turns
        .into_iter()
        .map(|(session_id, turns)| {
            let turns = turns.iter().map(TurnId::to_string).collect::<HashSet<_>>();
            (session_id.to_string(), turns)
        })
        .collect::<HashMap<_, _>>();
    let sessions = turns.keys().collect::<Vec<_>>();
    for sessions in sessions.chunks(SESSIONS_PER_ACTIVITY_READ) {
        let rows = activities::table
            .filter(activities::session_id.eq_any(sessions))
            .filter(diesel::dsl::sql::<diesel::sql_types::Bool>(
                "json_extract(payload, '$.kind') IN \
                 ('command', 'file_change', 'tool_call', 'subagent', \
                  'compaction', 'approval')",
            ))
            .select((
                activities::session_id,
                activities::turn_id,
                diesel::dsl::sql::<Text>("json_extract(payload, '$.kind')"),
                diesel::dsl::sql::<Nullable<Text>>("json_extract(payload, '$.status')"),
                diesel::dsl::sql::<Nullable<Text>>("json_extract(payload, '$.name')"),
                diesel::dsl::sql::<Nullable<Text>>("json_extract(payload, '$.server')"),
                diesel::dsl::sql::<Nullable<Text>>("json_extract(payload, '$.trigger')"),
                diesel::dsl::sql::<Nullable<BigInt>>("json_extract(payload, '$.before_tokens')"),
                diesel::dsl::sql::<Nullable<BigInt>>("json_extract(payload, '$.after_tokens')"),
                diesel::dsl::sql::<Nullable<Text>>("json_extract(payload, '$.outcome')"),
                diesel::dsl::sql::<Nullable<Text>>("json_extract(payload, '$.decision')"),
            ))
            .load_iter::<SelectedFields, diesel::connection::DefaultLoadingMode>(connection)
            .map_err(|error| StorageError::Read(error.to_string()))?;
        for fields in rows {
            let fields = fields.map_err(|error| StorageError::Read(error.to_string()))?;
            let named = turns
                .get(&fields.session_id)
                .is_some_and(|turns| turns.contains(&fields.turn_id));
            if let Some(row) = fields.into_row().filter(|_| named) {
                sink.add(row);
            }
        }
    }
    Ok(sink)
}
