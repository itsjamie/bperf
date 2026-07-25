//! Finalization and validation of measurement-local diagnostic artifacts.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    browser_lab::{
        ArtifactEvidence, ArtifactKind, Engine, validate_artifact_files,
        validate_trial_artifact_set,
    },
    measurement::{MeasurementSet, TrialResult, write_immutable},
    schedule::{MeasurementSchedule, TrialPhase},
};

const SCHEMA_VERSION: u32 = 1;
const POLICY: &str = "representative_per_workload_engine_v1";
const MANIFEST_NAME: &str = "artifact-retention.json";
const CPU_METRIC: &str = "browser.cpu_profile.active_ms";
const HEAP_METRIC: &str = "browser.js_heap.live_bytes";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactRetention {
    schema_version: u32,
    policy: String,
    measurement_set_id: String,
    selections: Vec<RetainedArtifact>,
    summary: RetentionSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedArtifact {
    workload_id: String,
    engine: Engine,
    trial_id: String,
    attempt: u32,
    representative_metric: String,
    median_value: f64,
    observed_value: f64,
    artifact: ArtifactEvidence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetentionSummary {
    pub(crate) policy: String,
    pub(crate) retained_artifacts: usize,
    pub(crate) discarded_artifacts: usize,
    pub(crate) retained_bytes: u64,
    pub(crate) discarded_bytes: u64,
}

pub(crate) fn load(root: &Path) -> Result<Option<ArtifactRetention>> {
    let path = root.join(MANIFEST_NAME);
    if !path.exists() {
        return Ok(None);
    }
    let manifest: ArtifactRetention = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("invalid {}", path.display()))?;
    if manifest.schema_version != SCHEMA_VERSION || manifest.policy != POLICY {
        bail!("unsupported artifact retention manifest {}", path.display());
    }
    Ok(Some(manifest))
}

pub(crate) fn finalize(measurement: &MeasurementSet) -> Result<Option<RetentionSummary>> {
    if measurement.needs_sampling_decision() || !measurement.pending_trials().is_empty() {
        return Ok(None);
    }

    let expected = build_manifest(measurement)?;
    let path = measurement.root().join(MANIFEST_NAME);
    if let Some(existing) = load(measurement.root())? {
        if let Some(mismatch) = manifest_mismatch(&existing, &expected) {
            bail!(
                "artifact retention manifest {} does not match the immutable trial evidence: {mismatch}",
                path.display(),
            );
        }
    } else {
        validate_retained_files(measurement.root(), &expected)?;
        write_immutable(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&expected)?).as_bytes(),
        )?;
    }
    prune_unselected(measurement.root(), &expected)?;
    Ok(Some(expected.summary))
}

pub(crate) fn validate(
    root: &Path,
    schedule: &MeasurementSchedule,
    completed: &HashMap<String, TrialResult>,
    manifest: &ArtifactRetention,
) -> Result<()> {
    let expected = build_manifest_from(schedule, completed)?;
    if let Some(mismatch) = manifest_mismatch(manifest, &expected) {
        bail!(
            "artifact retention manifest does not match the immutable trial evidence: {mismatch}"
        );
    }
    validate_retained_files(root, manifest)
}

pub(crate) fn validate_result(
    root: &Path,
    result: &TrialResult,
    retention_finalized: bool,
) -> Result<()> {
    if result.valid {
        validate_trial_artifact_set(result.engine, &result.artifacts)
            .with_context(|| format!("trial {} has invalid artifacts", result.trial_id))?;
        if retention_finalized {
            Ok(())
        } else {
            validate_artifact_files(result.engine, root, &result.artifacts)
                .with_context(|| format!("trial {} has invalid artifacts", result.trial_id))
        }
    } else if result.artifacts.is_empty() {
        Ok(())
    } else {
        bail!(
            "invalid trial {} cannot claim a complete artifact set",
            result.trial_id
        )
    }
}

fn build_manifest(measurement: &MeasurementSet) -> Result<ArtifactRetention> {
    build_manifest_from(&measurement.schedule, &measurement.results.completed)
}

fn build_manifest_from(
    schedule: &MeasurementSchedule,
    completed: &HashMap<String, TrialResult>,
) -> Result<ArtifactRetention> {
    let mut selections = Vec::new();
    let workloads: BTreeSet<_> = schedule
        .trials
        .iter()
        .filter(|trial| completed.contains_key(&trial.trial_id))
        .map(|trial| trial.workload_id.as_str())
        .collect();
    let engines: BTreeSet<_> = schedule
        .trials
        .iter()
        .filter(|trial| completed.contains_key(&trial.trial_id))
        .map(|trial| trial.engine)
        .collect();

    for workload_id in workloads {
        for engine in &engines {
            let results = preferred_results(schedule, completed, workload_id, *engine);
            if results.is_empty() {
                continue;
            }
            let cpu = representative(&results, CPU_METRIC)?;
            selections.push(selection(
                workload_id,
                *engine,
                cpu,
                CPU_METRIC,
                ArtifactKind::CpuProfile,
            )?);
            selections.push(selection(
                workload_id,
                *engine,
                cpu,
                CPU_METRIC,
                ArtifactKind::Flamegraph,
            )?);
            let heap = representative(&results, HEAP_METRIC)?;
            selections.push(selection(
                workload_id,
                *engine,
                heap,
                HEAP_METRIC,
                ArtifactKind::JsHeap,
            )?);
        }
    }
    selections.sort_by(|left, right| {
        (&left.workload_id, left.engine.as_str(), left.artifact.kind).cmp(&(
            &right.workload_id,
            right.engine.as_str(),
            right.artifact.kind,
        ))
    });

    let retained_artifacts = selections.len();
    let retained_bytes = sum_bytes(
        selections
            .iter()
            .map(|selection| selection.artifact.size_bytes),
    )?;
    let captured: Vec<_> = completed
        .values()
        .filter(|result| result.valid)
        .flat_map(|result| &result.artifacts)
        .collect();
    let total_artifacts = captured.len();
    let total_bytes = sum_bytes(captured.iter().map(|artifact| artifact.size_bytes))?;
    if retained_artifacts > total_artifacts || retained_bytes > total_bytes {
        bail!("artifact retention selection exceeds captured evidence");
    }

    Ok(ArtifactRetention {
        schema_version: SCHEMA_VERSION,
        policy: POLICY.to_owned(),
        measurement_set_id: schedule.measurement_set_id.clone(),
        selections,
        summary: RetentionSummary {
            policy: POLICY.to_owned(),
            retained_artifacts,
            discarded_artifacts: total_artifacts - retained_artifacts,
            retained_bytes,
            discarded_bytes: total_bytes - retained_bytes,
        },
    })
}

fn manifest_mismatch(stored: &ArtifactRetention, rebuilt: &ArtifactRetention) -> Option<String> {
    if stored.schema_version != rebuilt.schema_version
        || stored.policy != rebuilt.policy
        || stored.measurement_set_id != rebuilt.measurement_set_id
    {
        return Some("header changed".to_owned());
    }
    if stored.summary != rebuilt.summary {
        return Some("artifact totals changed".to_owned());
    }
    if stored.selections.len() != rebuilt.selections.len() {
        return Some("representative count changed".to_owned());
    }
    for (index, (stored, rebuilt)) in stored
        .selections
        .iter()
        .zip(&rebuilt.selections)
        .enumerate()
    {
        if stored.workload_id != rebuilt.workload_id
            || stored.engine != rebuilt.engine
            || stored.trial_id != rebuilt.trial_id
            || stored.attempt != rebuilt.attempt
            || stored.representative_metric != rebuilt.representative_metric
            || stored.artifact != rebuilt.artifact
        {
            return Some(format!("representative {} changed", index + 1));
        }
        if !nearly_equal(stored.median_value, rebuilt.median_value)
            || !nearly_equal(stored.observed_value, rebuilt.observed_value)
        {
            return Some(format!(
                "representative {} metric values changed",
                index + 1
            ));
        }
    }
    None
}

fn nearly_equal(left: f64, right: f64) -> bool {
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= scale * 1e-12
}

fn sum_bytes(values: impl IntoIterator<Item = u64>) -> Result<u64> {
    values.into_iter().try_fold(0u64, |total, value| {
        total
            .checked_add(value)
            .context("artifact byte total overflowed")
    })
}

fn preferred_results<'a>(
    schedule: &'a MeasurementSchedule,
    completed: &'a HashMap<String, TrialResult>,
    workload_id: &str,
    engine: Engine,
) -> Vec<&'a TrialResult> {
    for phase in [TrialPhase::Final, TrialPhase::Pilot, TrialPhase::Warmup] {
        let results: Vec<_> = schedule
            .trials
            .iter()
            .filter(|trial| {
                trial.workload_id == workload_id && trial.engine == engine && trial.phase == phase
            })
            .filter_map(|trial| completed.get(&trial.trial_id))
            .collect();
        if !results.is_empty() {
            return results;
        }
    }
    Vec::new()
}

fn representative<'a>(results: &[&'a TrialResult], metric: &str) -> Result<Representative<'a>> {
    let mut values = results
        .iter()
        .map(|result| {
            result
                .metrics
                .get(metric)
                .copied()
                .with_context(|| format!("trial {} has no {metric}", result.trial_id))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_by(f64::total_cmp);
    let median = if values.len() % 2 == 0 {
        let upper = values.len() / 2;
        (values[upper - 1] + values[upper]) / 2.0
    } else {
        values[values.len() / 2]
    };
    let result = results
        .iter()
        .copied()
        .min_by(|left, right| {
            let left_distance = (left.metrics[metric] - median).abs();
            let right_distance = (right.metrics[metric] - median).abs();
            left_distance
                .total_cmp(&right_distance)
                .then_with(|| left.trial_id.cmp(&right.trial_id))
        })
        .context("cannot select a representative from no trials")?;
    Ok(Representative {
        result,
        median,
        observed: result.metrics[metric],
    })
}

#[derive(Clone, Copy)]
struct Representative<'a> {
    result: &'a TrialResult,
    median: f64,
    observed: f64,
}

fn selection(
    workload_id: &str,
    engine: Engine,
    representative: Representative<'_>,
    metric: &str,
    kind: ArtifactKind,
) -> Result<RetainedArtifact> {
    let result = representative.result;
    let artifact = result
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == kind)
        .cloned()
        .with_context(|| format!("trial {} has no {kind:?} artifact", result.trial_id))?;

    Ok(RetainedArtifact {
        workload_id: workload_id.to_owned(),
        engine,
        trial_id: result.trial_id.clone(),
        attempt: result.attempt,
        representative_metric: metric.to_owned(),
        median_value: representative.median,
        observed_value: representative.observed,
        artifact,
    })
}

fn validate_retained_files(root: &Path, manifest: &ArtifactRetention) -> Result<()> {
    for selection in &manifest.selections {
        validate_artifact_files(
            selection.engine,
            root,
            std::slice::from_ref(&selection.artifact),
        )
        .with_context(|| {
            format!(
                "retained artifact {} for trial {} is invalid",
                selection.artifact.path, selection.trial_id
            )
        })?;
    }
    Ok(())
}

fn prune_unselected(root: &Path, manifest: &ArtifactRetention) -> Result<()> {
    let artifact_root = root.join("artifacts");
    if !artifact_root.exists() {
        return Ok(());
    }
    let measurement_root = fs::canonicalize(root)?;
    let artifact_root = fs::canonicalize(&artifact_root)?;
    if !artifact_root.starts_with(&measurement_root) {
        bail!("artifact directory escaped the measurement set");
    }
    let retained: HashSet<_> = manifest
        .selections
        .iter()
        .map(|selection| fs::canonicalize(root.join(&selection.artifact.path)))
        .collect::<std::io::Result<_>>()?;
    prune_directory(&artifact_root, &artifact_root, &retained, false)
}

fn prune_directory(
    root: &Path,
    path: &Path,
    retained: &HashSet<PathBuf>,
    remove_empty: bool,
) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!(
                "artifact cleanup refuses symbolic link {}",
                entry_path.display()
            );
        }
        let canonical = fs::canonicalize(&entry_path)?;
        if !canonical.starts_with(root) {
            bail!("artifact cleanup path escaped {}", root.display());
        }
        if file_type.is_dir() {
            prune_directory(root, &canonical, retained, true)?;
        } else {
            if !retained.contains(&canonical) {
                fs::remove_file(&entry_path)
                    .with_context(|| format!("failed to remove {}", entry_path.display()))?;
            }
        }
    }
    if remove_empty && fs::read_dir(path)?.next().is_none() {
        fs::remove_dir(path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::*;
    use crate::{
        measurement::{self, TrialResult},
        sampling::{BATCH_SIZE_METRIC, TRIAL_ELAPSED_METRIC},
    };

    #[test]
    fn completed_measurements_keep_median_representatives() {
        let directory = tempdir().unwrap();
        let root = measurement::prepare(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            Some(21),
            directory.path(),
        )
        .unwrap();
        let measurement = MeasurementSet::open(&root).unwrap();
        for trial in measurement.pending_trials() {
            let cpu = if trial.phase == TrialPhase::Final {
                f64::from(trial.sample_index)
            } else {
                11.0
            };
            let heap = if trial.phase == TrialPhase::Final {
                f64::from((trial.sample_index + 5) % 21 + 1)
            } else {
                11.0
            };
            measurement
                .append_result(&TrialResult {
                    schema_version: crate::MEASUREMENT_SCHEMA_VERSION,
                    measurement_set_id: measurement.measurement_set_id().to_owned(),
                    trial_id: trial.trial_id.clone(),
                    attempt: 1,
                    workload_id: trial.workload_id.clone(),
                    engine: trial.engine,
                    phase: trial.phase,
                    sample_index: trial.sample_index,
                    environment_fingerprint: "retention-test".to_owned(),
                    valid: true,
                    success: true,
                    failure_category: None,
                    failure_detail: None,
                    invalidation_reason: None,
                    metrics: BTreeMap::from([
                        ("workload.wall_ms".to_owned(), 20.0),
                        ("variant.call_wall_ms".to_owned(), 10.0),
                        (CPU_METRIC.to_owned(), cpu),
                        (HEAP_METRIC.to_owned(), heap),
                        (BATCH_SIZE_METRIC.to_owned(), 1.0),
                        (TRIAL_ELAPSED_METRIC.to_owned(), 20.0),
                    ]),
                    artifacts: synthetic_artifacts(
                        measurement.root(),
                        &trial.trial_id,
                        trial.engine,
                    ),
                })
                .unwrap();
        }

        let completed = MeasurementSet::open(&root).unwrap();
        let summary = finalize(&completed).unwrap().unwrap();
        let manifest = load(&root).unwrap().unwrap();
        assert_eq!(summary.retained_artifacts, manifest.selections.len());
        assert!(summary.discarded_artifacts > 0);
        assert_eq!(
            count_files(&root.join("artifacts")),
            summary.retained_artifacts
        );

        for selection in &manifest.selections {
            let expected_sample = match selection.artifact.kind {
                ArtifactKind::CpuProfile | ArtifactKind::Flamegraph => "0011",
                ArtifactKind::JsHeap => "0005",
            };
            assert!(selection.trial_id.ends_with(expected_sample));
            assert_eq!(selection.median_value, 11.0);
            assert_eq!(selection.observed_value, 11.0);
        }
        MeasurementSet::open(&root).unwrap();

        let mut changed = manifest.clone();
        changed.selections[0].trial_id.push_str("-different");
        let error = validate(
            &root,
            &completed.schedule,
            &completed.results.completed,
            &changed,
        )
        .unwrap_err();
        assert!(error.to_string().contains("representative 1 changed"));

        let missing = root.join(&manifest.selections[0].artifact.path);
        fs::remove_file(&missing).unwrap();
        let error = MeasurementSet::open(&root).err().unwrap();
        assert!(format!("{error:#}").contains("retained artifact"));
    }

    fn example(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join(name)
    }

    fn synthetic_artifacts(root: &Path, trial_id: &str, engine: Engine) -> Vec<ArtifactEvidence> {
        [
            ArtifactKind::CpuProfile,
            ArtifactKind::JsHeap,
            ArtifactKind::Flamegraph,
        ]
        .iter()
        .copied()
        .map(|kind| {
            let name = match kind {
                ArtifactKind::CpuProfile => "cpu",
                ArtifactKind::JsHeap => "heap",
                ArtifactKind::Flamegraph => "flamegraph",
            };
            let relative = PathBuf::from("artifacts")
                .join(trial_id)
                .join(format!("{name}.txt"));
            let path = root.join(&relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes = format!("{trial_id}-{engine}-{name}").into_bytes();
            fs::write(&path, &bytes).unwrap();
            ArtifactEvidence {
                kind,
                path: relative.to_string_lossy().replace('\\', "/"),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                format: "synthetic".to_owned(),
            }
        })
        .collect()
    }

    fn count_files(root: &Path) -> usize {
        if !root.exists() {
            return 0;
        }
        fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                if path.is_dir() { count_files(&path) } else { 1 }
            })
            .sum()
    }
}
