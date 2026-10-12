//! The directories this Server's Clones are made in, as the Server records
//! them (see [`StagingRecords`]): one row of `clone_staging` for each, its
//! path beside the Server run whose Clone made it. A path is kept as the
//! text it spells, so one that spells none is never recorded, and the Clone
//! that would have made it is refused instead.
//!
//! Each call opens its own connection and blocks on it, as a Clone records
//! and forgets from threads of its own; none touches a Session.

use diesel::prelude::*;

use super::{StorageRepository, connect};
use crate::source_control::{StagingRecord, StagingRecords};

diesel::table! {
    clone_staging (directory) {
        directory -> Text,
        run -> Text,
    }
}

#[derive(Queryable, Selectable, Insertable)]
#[diesel(table_name = clone_staging)]
struct StagingRow {
    directory: String,
    run: String,
}

impl StagingRecords for StorageRepository {
    fn record(&self, record: &StagingRecord) -> Result<(), String> {
        let directory = record.directory.to_str().ok_or_else(|| {
            format!(
                "`{}` spells no text the Server can record",
                record.directory.display()
            )
        })?;
        let mut connection = connect(&self.database_path).map_err(|error| error.to_string())?;
        diesel::replace_into(clone_staging::table)
            .values(StagingRow {
                directory: directory.to_owned(),
                run: record.run.clone(),
            })
            .execute(&mut connection)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn forget(&self, directory: &std::path::Path) -> Result<(), String> {
        let Some(directory) = directory.to_str() else {
            // Never recorded, so there is nothing to forget.
            return Ok(());
        };
        let mut connection = connect(&self.database_path).map_err(|error| error.to_string())?;
        diesel::delete(clone_staging::table.filter(clone_staging::directory.eq(directory)))
            .execute(&mut connection)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn recorded(&self) -> Result<Vec<StagingRecord>, String> {
        let mut connection = connect(&self.database_path).map_err(|error| error.to_string())?;
        Ok(clone_staging::table
            .select(StagingRow::as_select())
            .load::<StagingRow>(&mut connection)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|row| StagingRecord {
                directory: row.directory.into(),
                run: row.run,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory's record outlives the repository handle that made it, as
    /// it must outlive the Server run that made it, and is gone once
    /// forgotten; recording one again names the run that did so last.
    #[tokio::test]
    async fn a_staging_record_lasts_until_forgotten() {
        let data = tempfile::tempdir().unwrap();
        let staging = data.path().join(".atlas.suru-clone-0123abcd");
        let charts = data.path().join(".charts.suru-clone-89abcdef");
        {
            let repository = StorageRepository::open(data.path()).await.unwrap();
            for (directory, run) in [
                (&staging, "first"),
                (&charts, "first"),
                (&staging, "second"),
            ] {
                repository
                    .record(&StagingRecord {
                        directory: directory.clone(),
                        run: run.to_owned(),
                    })
                    .unwrap();
            }
        }
        let repository = StorageRepository::open(data.path()).await.unwrap();
        let mut recorded = repository.recorded().unwrap();
        recorded.sort_by(|one, other| one.directory.cmp(&other.directory));
        assert_eq!(
            recorded,
            [
                StagingRecord {
                    directory: staging.clone(),
                    run: "second".to_owned(),
                },
                StagingRecord {
                    directory: charts.clone(),
                    run: "first".to_owned(),
                },
            ]
        );
        repository.forget(&staging).unwrap();
        repository.forget(&staging).unwrap();
        assert_eq!(
            repository.recorded().unwrap(),
            [StagingRecord {
                directory: charts,
                run: "first".to_owned(),
            }]
        );
    }
}
