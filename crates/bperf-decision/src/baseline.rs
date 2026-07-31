//! Append-only baseline references.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bperf_measurement::store::MeasurementSet;
use serde::{Deserialize, Serialize};

use crate::environment;

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
    let previous = current_if_present(&registry_root, measurement.benchmark_id())?;
    if previous.as_ref().is_some_and(|record| {
        record.measurement_set_id == measurement.measurement_set_id()
            && record.measurement_set_path == measurement.root().to_string_lossy()
    }) {
        return Ok(previous.unwrap());
    }

    let record = BaselineRecord {
        schema_version: 2,
        benchmark_id: measurement.benchmark_id().to_owned(),
        subject_id: measurement.subject_id().to_owned(),
        benchmark_sha256: measurement.benchmark_sha256().to_owned(),
        measurement_set_id: measurement.measurement_set_id().to_owned(),
        measurement_set_path: measurement.root().to_string_lossy().into_owned(),
        variant_id: measurement.variant_id().to_owned(),
        variant_sha256: measurement.variant_sha256().to_owned(),
        environment_fingerprint: environment_fingerprint.to_owned(),
        measured_at_unix_ms,
        promoted_at_unix_ms: unix_time_ms()?,
        previous_measurement_set_id: previous.map(|record| record.measurement_set_id),
    };
    append(&registry_root, &record)?;
    Ok(record)
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
    Ok(PathBuf::from(
        current(registry_root, benchmark_id)?.measurement_set_path,
    ))
}

pub fn current_path_if_present(
    registry_root: &Path,
    benchmark_id: &str,
) -> Result<Option<PathBuf>> {
    Ok(current_if_present(registry_root, benchmark_id)?
        .map(|record| PathBuf::from(record.measurement_set_path)))
}

fn current_if_present(registry_root: &Path, benchmark_id: &str) -> Result<Option<BaselineRecord>> {
    if !registry_path(registry_root, benchmark_id).exists() {
        return Ok(None);
    }
    current(registry_root, benchmark_id).map(Some)
}

fn current(registry_root: &Path, benchmark_id: &str) -> Result<BaselineRecord> {
    let path = registry_path(registry_root, benchmark_id);
    if !path.exists() {
        bail!("no promoted baseline for benchmark {benchmark_id:?}");
    }
    let record: BaselineRecord = bperf_storage::read_last_json_line(&path)
        .with_context(|| format!("invalid baseline history {}", path.display()))?
        .with_context(|| format!("baseline history {} is empty", path.display()))?;
    if record.schema_version != 2 || record.benchmark_id != benchmark_id {
        bail!(
            "baseline history {} has incompatible identity; promote a measurement made with the current runtime-anchor protocol",
            path.display()
        );
    }
    Ok(record)
}

fn append(registry_root: &Path, record: &BaselineRecord) -> Result<()> {
    let path = registry_path(registry_root, &record.benchmark_id);
    bperf_storage::append_json_line(&path, record)
        .with_context(|| format!("failed to append baseline history {}", path.display()))
}

fn registry_path(registry_root: &Path, benchmark_id: &str) -> PathBuf {
    registry_root.join(format!("{benchmark_id}.jsonl"))
}

#[derive(Debug, Deserialize, Serialize)]
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
}

#[cfg(test)]
mod tests {
    use std::io::Write;

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
    fn interrupted_baseline_append_is_ignored_and_replaced_on_resume() {
        let directory = tempdir().unwrap();
        append(directory.path(), &record("first", None)).unwrap();
        let path = registry_path(directory.path(), "benchmark");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"schema_version":"#)
            .unwrap();

        assert_eq!(
            current(directory.path(), "benchmark")
                .unwrap()
                .measurement_set_id,
            "first"
        );
        append(directory.path(), &record("second", Some("first"))).unwrap();
        assert_eq!(
            current(directory.path(), "benchmark")
                .unwrap()
                .measurement_set_id,
            "second"
        );
    }
}
