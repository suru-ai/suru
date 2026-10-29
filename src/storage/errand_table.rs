//! The Errands this Server ran, as the `errands` table records them
//! (ADR 0057): one row for each Errand that reached a Provider, landed by the
//! storage writer as it ended, and read back by when it began. No row is
//! loaded at startup; a reader asks for the interval it counts, on a blocking
//! task of its own.

use std::ops::Range;

use diesel::prelude::*;

use serde::{Serialize, de::DeserializeOwned};

use super::{Spelled, StorageError, StorageRepository, connect, on_blocking_task, stamp_column};
use crate::{
    errands::ErrandRecord,
    protocol::{Cost, ModelId, ProviderId, SessionTimestamp, Usage},
    provider::ErrandCost,
};

diesel::table! {
    errands (id) {
        id -> BigInt,
        purpose -> Text,
        provider -> Text,
        model -> Text,
        began_at -> BigInt,
        ended_at -> BigInt,
        outcome -> Text,
        usage -> Nullable<Text>,
        cost_nano_usd -> Nullable<BigInt>,
        cost_basis -> Nullable<Text>,
    }
}

/// One Errand's row as written; its id is the row's own.
#[derive(Insertable)]
#[diesel(table_name = errands)]
struct NewErrandRow {
    purpose: String,
    provider: String,
    model: String,
    began_at: i64,
    ended_at: i64,
    outcome: String,
    usage: Option<String>,
    cost_nano_usd: Option<i64>,
    cost_basis: Option<String>,
}

/// One Errand's row as read, in the order the table declares its columns.
#[derive(Queryable, Selectable)]
#[diesel(table_name = errands)]
struct ErrandRow {
    id: i64,
    purpose: String,
    provider: String,
    model: String,
    began_at: i64,
    ended_at: i64,
    outcome: String,
    usage: Option<String>,
    cost_nano_usd: Option<i64>,
    cost_basis: Option<String>,
}

impl NewErrandRow {
    fn of(record: &ErrandRecord) -> Result<Self, StorageError> {
        let usage = record
            .usage
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| StorageError::WriteErrand(error.to_string()))?;
        Ok(Self {
            purpose: spelling(record.purpose)?,
            provider: record.provider.as_str().to_owned(),
            model: record.model.as_str().to_owned(),
            began_at: stamp_column(record.began),
            ended_at: stamp_column(record.ended),
            outcome: spelling(record.outcome)?,
            usage,
            cost_nano_usd: record
                .cost
                .map(|cost| i64::try_from(cost.cost().nano_usd()).unwrap_or(i64::MAX)),
            cost_basis: record.cost.map(|cost| spelling(cost.basis())).transpose()?,
        })
    }
}

impl ErrandRow {
    /// The record the row spells, or why this Suru reads none from it.
    fn into_record(self) -> Result<ErrandRecord, String> {
        let purpose = spelled(self.purpose, "purpose")?;
        let outcome = spelled(self.outcome, "outcome")?;
        let usage = self
            .usage
            .map(|usage| serde_json::from_str::<Usage>(&usage))
            .transpose()
            .map_err(|error| format!("its Usage does not decode: {error}"))?;
        let cost = match (self.cost_nano_usd, self.cost_basis) {
            (Some(nano_usd), Some(basis)) => Some(ErrandCost::as_recorded(
                Cost::from_nano_usd(
                    u64::try_from(nano_usd).map_err(|_| "its Cost is negative".to_owned())?,
                ),
                spelled(basis, "Cost Basis")?,
            )),
            (None, None) => None,
            _ => return Err("its Cost and Cost Basis disagree on being known".to_owned()),
        };
        Ok(ErrandRecord {
            purpose,
            provider: ProviderId::new(self.provider),
            model: ModelId::new(self.model),
            began: timestamp(self.began_at),
            ended: timestamp(self.ended_at),
            outcome,
            usage,
            cost,
        })
    }
}

fn timestamp(column: i64) -> SessionTimestamp {
    SessionTimestamp(u64::try_from(column).unwrap_or(0))
}

/// How a column spells `value`: as serde spells it, so the purpose, outcome,
/// and Cost Basis are spelled the one way their types already say.
fn spelling(value: impl Serialize) -> Result<String, StorageError> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(spelling)) => Ok(spelling),
        Ok(other) => Err(StorageError::WriteErrand(format!(
            "`{other}` spells no column"
        ))),
        Err(error) => Err(StorageError::WriteErrand(error.to_string())),
    }
}

/// The `T` a column spells as serde spells it, or why it spells none; `what`
/// names the column for the Log.
fn spelled<T: DeserializeOwned>(column: String, what: &str) -> Result<T, String> {
    match Spelled::of(Some(column)) {
        Spelled::Known(value) => Ok(value),
        Spelled::Unknown(column) => Err(format!("no {what} is spelled `{column}`")),
        Spelled::Absent => Err(format!("it has no {what}")),
    }
}

impl StorageRepository {
    /// Records one Errand that reached a Provider. Only the storage writer
    /// calls this, so an Errand's answer never waits on it.
    pub(super) fn record_errand(&self, record: &ErrandRecord) -> Result<(), StorageError> {
        let row = NewErrandRow::of(record)?;
        let mut connection = connect(&self.database_path)?;
        diesel::insert_into(errands::table)
            .values(&row)
            .execute(&mut connection)
            .map(|_| ())
            .map_err(|error| StorageError::WriteErrand(error.to_string()))
    }

    /// Every Errand recorded as beginning within `began` — from its start,
    /// up to but not including its end — earliest first, read on a blocking
    /// task through the index on when each began.
    ///
    /// A row this Suru cannot read is logged and left out rather than failing
    /// the read. The schema version only guards the table's shape: a later
    /// Suru that adds a purpose or outcome writes it without a migration, so
    /// after a downgrade this Suru can meet a spelling it does not know, and a
    /// figure missing one Errand is better than no figures at all.
    pub(crate) async fn errand_records(
        &self,
        began: Range<SessionTimestamp>,
    ) -> Result<Vec<ErrandRecord>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("reading Errand records", move || {
            let mut connection = connect(&path)?;
            let rows = errands::table
                .filter(errands::began_at.ge(stamp_column(began.start)))
                .filter(errands::began_at.lt(stamp_column(began.end)))
                .order((errands::began_at, errands::id))
                .select(ErrandRow::as_select())
                .load::<ErrandRow>(&mut connection)
                .map_err(|error| StorageError::Read(error.to_string()))?;
            Ok(rows
                .into_iter()
                .filter_map(|row| {
                    let id = row.id;
                    row.into_record()
                        .inspect_err(|reason| {
                            tracing::warn!("left out Errand record {id}: {reason}");
                        })
                        .ok()
                })
                .collect())
        })
        .await
    }

    /// When the earliest Errand recorded began, read on a blocking task
    /// through the index on when each began; none where none is recorded.
    pub(crate) async fn earliest_errand_began(
        &self,
    ) -> Result<Option<SessionTimestamp>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("reading when the earliest Errand began", move || {
            let mut connection = connect(&path)?;
            let earliest = errands::table
                .select(diesel::dsl::min(errands::began_at))
                .first::<Option<i64>>(&mut connection)
                .map_err(|error| StorageError::Read(error.to_string()))?;
            Ok(earliest.map(timestamp))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        errands::{ErrandOutcome, ErrandPurpose},
        protocol::CostBasis,
        storage::StorageWriter,
    };

    fn at(millis: u64) -> SessionTimestamp {
        SessionTimestamp(millis)
    }

    fn record(began: u64, purpose: ErrandPurpose) -> ErrandRecord {
        ErrandRecord {
            purpose,
            provider: ProviderId::new("claude"),
            model: ModelId::new("claude-haiku-4-5"),
            began: at(began),
            ended: at(began + 2_400),
            outcome: ErrandOutcome::Answered,
            usage: Some(Usage {
                fresh_input_tokens: Some(12),
                cache_read_tokens: Some(14_211),
                cache_write_tokens: Some(1_830),
                output_tokens: Some(56),
                reasoning_tokens: Some(40),
                native_meter: None,
            }),
            cost: Some(ErrandCost::reported(Cost::from_nano_usd(4_123_500))),
        }
    }

    /// Records outlive the repository that wrote them, and are read back
    /// whole by when they began: the interval's start counts, its end does
    /// not, and those either side are left out. The earliest of them says
    /// when it began.
    #[tokio::test]
    async fn errand_records_are_read_back_by_when_they_began() {
        let data = tempfile::tempdir().unwrap();
        let unmeasured = ErrandRecord {
            outcome: ErrandOutcome::TimedOut,
            usage: None,
            cost: None,
            ..record(2_000, ErrandPurpose::TitleProposal)
        };
        let estimated = ErrandRecord {
            outcome: ErrandOutcome::Failed,
            cost: Some(ErrandCost::as_recorded(
                Cost::from_nano_usd(900),
                CostBasis::Estimated,
            )),
            ..record(1_500, ErrandPurpose::WorkspaceDescription)
        };
        {
            let repository = StorageRepository::open(data.path()).await.unwrap();
            assert_eq!(repository.earliest_errand_began().await.unwrap(), None);
            for written in [
                record(3_000, ErrandPurpose::WorkspaceDescription),
                record(999, ErrandPurpose::SessionTitle),
                unmeasured.clone(),
                estimated.clone(),
                record(1_000, ErrandPurpose::SessionTitle),
            ] {
                repository.record_errand(&written).unwrap();
            }
        }
        let repository = StorageRepository::open(data.path()).await.unwrap();

        assert_eq!(
            repository
                .errand_records(at(1_000)..at(3_000))
                .await
                .unwrap(),
            vec![
                record(1_000, ErrandPurpose::SessionTitle),
                estimated,
                unmeasured
            ]
        );
        assert_eq!(
            repository
                .errand_records(at(3_001)..at(u64::MAX))
                .await
                .unwrap(),
            Vec::new()
        );
        assert_eq!(
            repository.earliest_errand_began().await.unwrap(),
            Some(at(999))
        );
    }

    /// The runner hands its records to the storage writer, which lands them
    /// on its own thread.
    #[tokio::test]
    async fn the_storage_writer_lands_an_errand_record() {
        use crate::errands::ErrandLedger;

        let data = tempfile::tempdir().unwrap();
        let repository = StorageRepository::open(data.path()).await.unwrap();
        let (writer, sink) = StorageWriter::spawn(repository.clone());

        sink.record(record(1_000, ErrandPurpose::SessionTitle))
            .expect("the writer is running");
        writer.shutdown().await.unwrap();

        assert_eq!(
            repository
                .errand_records(at(0)..at(u64::MAX))
                .await
                .unwrap(),
            vec![record(1_000, ErrandPurpose::SessionTitle)]
        );
    }
}
