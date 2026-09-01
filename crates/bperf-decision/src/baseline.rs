//! Append-only baseline references.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bperf_measurement::store::MeasurementSet;
use bperf_storage::database::{Database, DatabaseReader, WriteTransaction};
use serde::{Deserialize, Serialize};

use crate::environment;

pub(crate) const BASELINE_EVENTS: &str = "baseline";

pub struct PromoteOptions {
    pub measurement_root: PathBuf,
    pub registry_root: PathBuf,
    pub json: bool,
}

pub fn promote(options: PromoteOptions) -> Result<()> {
    let record = promote_measurement(&options.measurement_root, &options.registry_root)?;
    emit(&record, options.json)
}

pub(crate) fn promote_measurement(
    measurement_root: &Path,
    registry_root: &Path,
) -> Result<BaselineRecord> {
    let pending = prepare_measurement(measurement_root)?;
    let database = promotion_database(registry_root)?;
    database.write(|transaction| promote_prepared(transaction, &pending))
}

pub(crate) fn prepare_measurement(measurement_root: &Path) -> Result<PendingBaseline> {
    let measurement = MeasurementSet::open(measurement_root)?;
    if !measurement.final_is_complete() {
        bail!(
            "measurement set {} is incomplete and cannot become a baseline",
            measurement.measurement_set_id()
        );
    }
    let environment_fingerprint = measurement
        .environment_fingerprint()
        .context("complete measurement set has no environment fingerprint")?;
    let measured_at_unix_ms = environment::read(&measurement)?.recorded_at_unix_ms();
    Ok(PendingBaseline {
        benchmark_id: measurement.benchmark_id().to_owned(),
        subject_id: measurement.subject_id().to_owned(),
        benchmark_sha256: measurement.benchmark_sha256().to_owned(),
        measurement_set_id: measurement.measurement_set_id().to_owned(),
        measurement_set_path: measurement.root().to_string_lossy().into_owned(),
        variant_id: measurement.variant_id().to_owned(),
        variant_sha256: measurement.variant_sha256().to_owned(),
        environment_fingerprint: environment_fingerprint.to_owned(),
        measured_at_unix_ms,
    })
}

pub(crate) fn promote_prepared(
    transaction: &mut WriteTransaction<'_>,
    pending: &PendingBaseline,
) -> Result<BaselineRecord> {
    let history: Vec<BaselineRecord> =
        transaction.read_events(BASELINE_EVENTS, &pending.benchmark_id)?;
    validate_history(&history, &pending.benchmark_id)?;
    let previous = history.last().cloned();
    if let Some(previous) = previous
        .as_ref()
        .filter(|record| record.measurement_set_id == pending.measurement_set_id)
    {
        if !previous.matches_measurement(pending) {
            bail!("existing baseline reference does not match the immutable measurement");
        }
        return Ok(previous.clone());
    }

    let record = BaselineRecord {
        schema_version: 2,
        benchmark_id: pending.benchmark_id.clone(),
        subject_id: pending.subject_id.clone(),
        benchmark_sha256: pending.benchmark_sha256.clone(),
        measurement_set_id: pending.measurement_set_id.clone(),
        measurement_set_path: pending.measurement_set_path.clone(),
        variant_id: pending.variant_id.clone(),
        variant_sha256: pending.variant_sha256.clone(),
        environment_fingerprint: pending.environment_fingerprint.clone(),
        measured_at_unix_ms: pending.measured_at_unix_ms,
        promoted_at_unix_ms: unix_time_ms()?,
        previous_measurement_set_id: previous.map(|record| record.measurement_set_id),
    };
    transaction.append_event(
        BASELINE_EVENTS,
        &record.benchmark_id,
        &serde_json::to_vec(&record)?,
    )?;
    Ok(record)
}

pub(crate) struct PendingBaseline {
    benchmark_id: String,
    subject_id: String,
    benchmark_sha256: String,
    measurement_set_id: String,
    measurement_set_path: String,
    variant_id: String,
    variant_sha256: String,
    environment_fingerprint: String,
    measured_at_unix_ms: u64,
}

impl PendingBaseline {
    pub(crate) fn benchmark_id(&self) -> &str {
        &self.benchmark_id
    }
}

pub struct ShowOptions {
    pub benchmark_id: String,
    pub registry_root: PathBuf,
    pub json: bool,
}

pub fn show(options: ShowOptions) -> Result<()> {
    let record = current(&options.registry_root, &options.benchmark_id)?;
    emit(&record, options.json)
}

fn emit(record: &BaselineRecord, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(record)?);
    } else {
        println!("bperf baseline: {}", record.variant_id);
        println!("  benchmark: {}", record.benchmark_id);
        println!("  measurement set: {}", record.measurement_set_id);
        println!("  path: {}", record.measurement_set_path);
        println!("  measured at: {}", record.measured_at_unix_ms);
        println!("  promoted at: {}", record.promoted_at_unix_ms);
    }
    Ok(())
}

pub fn current_path(registry_root: &Path, benchmark_id: &str) -> Result<PathBuf> {
    validated_measurement_path(&current(registry_root, benchmark_id)?)
}

pub fn current_path_if_present(
    registry_root: &Path,
    benchmark_id: &str,
) -> Result<Option<PathBuf>> {
    current_if_present(registry_root, benchmark_id)?
        .as_ref()
        .map(validated_measurement_path)
        .transpose()
}

fn validated_measurement_path(record: &BaselineRecord) -> Result<PathBuf> {
    let path = PathBuf::from(&record.measurement_set_path);
    let pending = prepare_measurement(&path).with_context(|| {
        format!(
            "failed to validate baseline measurement {:?}",
            record.measurement_set_id
        )
    })?;
    if !record.matches_measurement(&pending) {
        bail!("baseline reference does not match its immutable measurement");
    }
    Ok(path)
}

fn current_if_present(registry_root: &Path, benchmark_id: &str) -> Result<Option<BaselineRecord>> {
    let database = baseline_database(registry_root)?;
    read_current(&database, benchmark_id)
}

fn current(registry_root: &Path, benchmark_id: &str) -> Result<BaselineRecord> {
    let database = baseline_database(registry_root)?;
    read_current(&database, benchmark_id)?
        .with_context(|| format!("no promoted baseline for benchmark {benchmark_id:?}"))
}

fn read_current(database: &Database, benchmark_id: &str) -> Result<Option<BaselineRecord>> {
    let reader = database.reader()?;
    let mut history = read_history_with(&reader, benchmark_id)?;
    Ok(history.pop())
}

pub(crate) fn read_history_with(
    reader: &DatabaseReader,
    benchmark_id: &str,
) -> Result<Vec<BaselineRecord>> {
    let history = reader.read_events(BASELINE_EVENTS, benchmark_id)?;
    validate_history(&history, benchmark_id)?;
    Ok(history)
}

fn validate_history(history: &[BaselineRecord], benchmark_id: &str) -> Result<()> {
    let mut previous = None;
    for record in history {
        if record.schema_version != 2 || record.benchmark_id != benchmark_id {
            bail!(
                "baseline history for {benchmark_id:?} has incompatible identity; promote a measurement made with the current runtime-anchor protocol"
            );
        }
        if [
            &record.subject_id,
            &record.benchmark_sha256,
            &record.measurement_set_id,
            &record.measurement_set_path,
            &record.variant_id,
            &record.variant_sha256,
            &record.environment_fingerprint,
        ]
        .iter()
        .any(|value| value.trim().is_empty())
            || record.measured_at_unix_ms == 0
            || record.promoted_at_unix_ms == 0
        {
            bail!("baseline history for {benchmark_id:?} contains an incomplete reference");
        }
        if Path::new(&record.measurement_set_path)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(record.measurement_set_id.as_str())
        {
            bail!(
                "baseline history for {benchmark_id:?} has a measurement path inconsistent with {:?}",
                record.measurement_set_id
            );
        }
        if record.previous_measurement_set_id.as_deref() != previous {
            bail!(
                "baseline history for {benchmark_id:?} has a broken predecessor before {:?}",
                record.measurement_set_id
            );
        }
        if record.previous_measurement_set_id.as_deref() == Some(record.measurement_set_id.as_str())
        {
            bail!(
                "baseline history for {benchmark_id:?} repeats the current measurement {:?}",
                record.measurement_set_id
            );
        }
        previous = Some(record.measurement_set_id.as_str());
    }
    Ok(())
}

#[cfg(test)]
fn append(registry_root: &Path, record: &BaselineRecord) -> Result<()> {
    let database = baseline_database(registry_root)?;
    database
        .append_event(BASELINE_EVENTS, &record.benchmark_id, record)
        .map(|_| ())
}

pub(crate) fn promotion_database(registry_root: &Path) -> Result<Database> {
    fs::create_dir_all(registry_root).with_context(|| {
        format!(
            "failed to create baseline registry {}",
            registry_root.display()
        )
    })?;
    let registry_root = fs::canonicalize(registry_root).with_context(|| {
        format!(
            "failed to resolve baseline registry {}",
            registry_root.display()
        )
    })?;
    baseline_database(&registry_root)
}

fn baseline_database(registry_root: &Path) -> Result<Database> {
    Database::for_collection(registry_root, "baselines")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BaselineRecord {
    schema_version: u32,
    benchmark_id: String,
    subject_id: String,
    benchmark_sha256: String,
    measurement_set_id: String,
    measurement_set_path: String,
    variant_id: String,
    variant_sha256: String,
    environment_fingerprint: String,
    measured_at_unix_ms: u64,
    promoted_at_unix_ms: u64,
    previous_measurement_set_id: Option<String>,
}

fn unix_time_ms() -> Result<u64> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(milliseconds).context("system time does not fit in baseline timestamp")
}

impl BaselineRecord {
    pub(crate) fn measurement_set_id(&self) -> &str {
        &self.measurement_set_id
    }

    pub(crate) fn previous_measurement_set_id(&self) -> Option<&str> {
        self.previous_measurement_set_id.as_deref()
    }

    pub(crate) fn matches_transition(&self, measurement_set: &str, previous: Option<&str>) -> bool {
        self.measurement_set_id == measurement_set
            && self.previous_measurement_set_id.as_deref() == previous
    }

    fn matches_measurement(&self, pending: &PendingBaseline) -> bool {
        self.benchmark_id == pending.benchmark_id
            && self.subject_id == pending.subject_id
            && self.benchmark_sha256 == pending.benchmark_sha256
            && self.measurement_set_id == pending.measurement_set_id
            && self.measurement_set_path == pending.measurement_set_path
            && self.variant_id == pending.variant_id
            && self.variant_sha256 == pending.variant_sha256
            && self.environment_fingerprint == pending.environment_fingerprint
            && self.measured_at_unix_ms == pending.measured_at_unix_ms
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn record(id: &str, previous: Option<&str>) -> BaselineRecord {
        BaselineRecord {
            schema_version: 2,
            benchmark_id: "benchmark".to_owned(),
            subject_id: "subject".to_owned(),
            benchmark_sha256: "benchmark-hash".to_owned(),
            measurement_set_id: id.to_owned(),
            measurement_set_path: format!("C:/measurements/{id}"),
            variant_id: id.to_owned(),
            variant_sha256: format!("{id}-hash"),
            environment_fingerprint: "environment".to_owned(),
            measured_at_unix_ms: 1,
            promoted_at_unix_ms: 2,
            previous_measurement_set_id: previous.map(str::to_owned),
        }
    }

    #[test]
    fn latest_append_is_the_current_baseline() {
        let directory = tempdir().unwrap();
        append(directory.path(), &record("first", None)).unwrap();
        append(directory.path(), &record("second", Some("first"))).unwrap();
        let current = current(directory.path(), "benchmark").unwrap();
        assert_eq!(current.measurement_set_id, "second");
        assert_eq!(
            current.previous_measurement_set_id.as_deref(),
            Some("first")
        );
    }

    #[test]
    fn baseline_history_uses_atomic_database_events() {
        let directory = tempdir().unwrap();
        append(directory.path(), &record("first", None)).unwrap();
        assert_eq!(
            current(directory.path(), "benchmark")
                .unwrap()
                .measurement_set_id,
            "first"
        );
        assert!(!directory.path().join("benchmark.jsonl").exists());
        append(directory.path(), &record("second", Some("first"))).unwrap();
        assert_eq!(
            current(directory.path(), "benchmark")
                .unwrap()
                .measurement_set_id,
            "second"
        );
    }

    #[test]
    fn baseline_reads_reject_a_broken_history_chain() {
        let directory = tempdir().unwrap();
        append(directory.path(), &record("first", None)).unwrap();
        append(directory.path(), &record("second", Some("unrelated"))).unwrap();

        let error = current_path_if_present(directory.path(), "benchmark").unwrap_err();
        assert!(
            error.to_string().contains("broken predecessor"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn baseline_reads_reject_a_path_for_another_measurement() {
        let directory = tempdir().unwrap();
        let mut mismatched = record("first", None);
        mismatched.measurement_set_path = "C:/measurements/second".to_owned();
        append(directory.path(), &mismatched).unwrap();

        let error = current_path_if_present(directory.path(), "benchmark").unwrap_err();
        assert!(
            error.to_string().contains("measurement path inconsistent"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn one_measurement_identity_cannot_move_between_paths() {
        let directory = tempdir().unwrap();
        let existing = record("first", None);
        append(directory.path(), &existing).unwrap();
        let pending = PendingBaseline {
            benchmark_id: existing.benchmark_id.clone(),
            subject_id: existing.subject_id.clone(),
            benchmark_sha256: existing.benchmark_sha256.clone(),
            measurement_set_id: existing.measurement_set_id.clone(),
            measurement_set_path: "C:/elsewhere/first".to_owned(),
            variant_id: existing.variant_id.clone(),
            variant_sha256: existing.variant_sha256.clone(),
            environment_fingerprint: existing.environment_fingerprint.clone(),
            measured_at_unix_ms: existing.measured_at_unix_ms,
        };
        let database = promotion_database(directory.path()).unwrap();

        let error = database
            .write(|transaction| promote_prepared(transaction, &pending))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match the immutable measurement"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            current(directory.path(), "benchmark")
                .unwrap()
                .measurement_set_path,
            existing.measurement_set_path
        );
    }

    #[test]
    fn baseline_reads_reject_a_redundant_self_transition() {
        let directory = tempdir().unwrap();
        append(directory.path(), &record("first", None)).unwrap();
        append(directory.path(), &record("first", Some("first"))).unwrap();

        let error = current_path_if_present(directory.path(), "benchmark").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("repeats the current measurement"),
            "unexpected error: {error:#}"
        );
    }
}
