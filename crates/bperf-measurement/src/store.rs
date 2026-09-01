//! Measurement-set preparation, loading, and result validation.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::{ArtifactEvidence, Engine, TrialBatchConfig};
use bperf_storage::database::Database;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    MEASUREMENT_SCHEMA_VERSION,
    manifest::{AnalysisPolicy, BenchmarkManifest, VariantDescriptor},
    retention::{self as artifact_retention, ArtifactRetention},
    sampling::{self, MAX_BATCH_SIZE, PROFILE_BATCH_TARGET_MS, RunBudget, SamplingDecision},
    schedule::{MeasurementSchedule, SamplingSchedule, ScheduledTrial, TrialPhase},
};

const PREFLIGHT_CAPTURE_DIRECTORY: &str = "preflight";
const FROZEN_WORKLOAD_DIRECTORY: &str = "workloads";
const MEASUREMENT_DOCUMENTS: &str = "measurement";
const MEASUREMENT_TRIALS: &str = "measurement_trials";

pub struct ValidateOptions {
    pub benchmark: PathBuf,
    pub variant: Option<PathBuf>,
    pub json: bool,
}

pub fn validate(options: ValidateOptions) -> Result<()> {
    let benchmark = BenchmarkManifest::load(&options.benchmark)?;
    let variant = options
        .variant
        .as_deref()
        .map(VariantDescriptor::load)
        .transpose()?;
    if let Some(variant) = &variant {
        benchmark.validate_variant(variant)?;
    }

    let summary = ValidationSummary::new(&benchmark, variant.as_ref());
    if options.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("bperf validate: valid");
        println!("  benchmark: {}", benchmark.benchmark_id());
        println!("  subject: {}", benchmark.subject_id());
        if let Some(variant) = &variant {
            println!("  variant: {}", variant.id());
        }
        println!(
            "  workloads: {}",
            benchmark.workload_ids().collect::<Vec<_>>().join(", ")
        );
        println!(
            "  engines: {}",
            benchmark
                .engines()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("  benchmark sha256: {}", benchmark.source_sha256());
    }
    Ok(())
}

pub struct PlanOptions {
    pub benchmark: PathBuf,
    pub variant: PathBuf,
    pub final_samples: Option<u32>,
    pub artifact_root: PathBuf,
    pub json: bool,
}

pub fn plan(options: PlanOptions) -> Result<()> {
    let measurement_root = prepare(
        &options.benchmark,
        &options.variant,
        options.final_samples,
        &options.artifact_root,
    )?;
    let measurement = MeasurementSet::open(&measurement_root)?;
    let final_samples = measurement.schedule.final_samples;
    let summary = PlanSummary {
        schema_version: MEASUREMENT_SCHEMA_VERSION,
        measurement_set_id: measurement.measurement_set_id().to_owned(),
        benchmark_id: measurement.benchmark_id(),
        subject_id: measurement.subject_id(),
        variant_id: measurement.variant_id(),
        benchmark_sha256: measurement.benchmark_sha256(),
        variant_sha256: measurement.variant_sha256(),
        measurement_root: measurement_root.to_string_lossy(),
        trial_count: measurement.schedule.trials.len(),
        final_trial_count: measurement.schedule.final_trial_count(),
        final_samples,
    };
    if options.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("bperf plan: ready");
        println!("  measurement set: {}", summary.measurement_set_id);
        println!(
            "  {} total trials / {} final trials",
            summary.trial_count, summary.final_trial_count
        );
        println!(
            "  {} final samples per workload and engine",
            summary.final_samples
        );
        println!("  artifacts: {}", measurement_root.display());
    }
    Ok(())
}

pub fn prepare(
    benchmark_path: &Path,
    variant_path: &Path,
    requested_final_samples: Option<u32>,
    artifact_root: &Path,
) -> Result<PathBuf> {
    prepare_with_sampling(
        benchmark_path,
        variant_path,
        artifact_root,
        SamplingRequest::Fixed(requested_final_samples),
    )
}

pub fn prepare_adaptive(
    benchmark_path: &Path,
    variant_path: &Path,
    budget: RunBudget,
    cohort: Option<&str>,
    artifact_root: &Path,
) -> Result<PathBuf> {
    prepare_with_sampling(
        benchmark_path,
        variant_path,
        artifact_root,
        SamplingRequest::Adaptive(budget, cohort.map(str::to_owned)),
    )
}

enum SamplingRequest {
    Fixed(Option<u32>),
    Adaptive(RunBudget, Option<String>),
}

fn prepare_with_sampling(
    benchmark_path: &Path,
    variant_path: &Path,
    artifact_root: &Path,
    sampling: SamplingRequest,
) -> Result<PathBuf> {
    let benchmark = BenchmarkManifest::load(benchmark_path)?;
    let variant = VariantDescriptor::load(variant_path)?;
    benchmark.validate_variant(&variant)?;
    let (measurement_set_id, schedule) = match sampling {
        SamplingRequest::Fixed(requested) => {
            let final_samples = benchmark.resolve_final_samples(requested)?;
            let measurement_set_id = fixed_measurement_set_id(&benchmark, &variant, final_samples);
            let schedule = MeasurementSchedule::build(
                &benchmark,
                &variant,
                measurement_set_id.clone(),
                final_samples,
            );
            (measurement_set_id, schedule)
        }
        SamplingRequest::Adaptive(budget, cohort) => {
            if cohort.as_ref().is_some_and(|value| value.trim().is_empty()) {
                bail!("measurement cohort must not be empty");
            }
            let (min_final_samples, max_final_samples) = benchmark.adaptive_final_sample_range()?;
            let measurement_set_id = adaptive_measurement_set_id(
                &benchmark,
                &variant,
                budget.milliseconds(),
                cohort.as_deref(),
            );
            let schedule = MeasurementSchedule::build_adaptive(
                &benchmark,
                &variant,
                measurement_set_id.clone(),
                budget.milliseconds(),
                min_final_samples,
                max_final_samples,
                cohort,
            );
            (measurement_set_id, schedule)
        }
    };
    let measurement_root = artifact_root.join(&measurement_set_id);
    fs::create_dir_all(&measurement_root).with_context(|| {
        format!(
            "failed to create measurement directory {}",
            measurement_root.display()
        )
    })?;
    let measurement_root = fs::canonicalize(&measurement_root).with_context(|| {
        format!(
            "failed to resolve measurement directory {}",
            measurement_root.display()
        )
    })?;
    let database = Database::for_collection(artifact_root, "measurements")?;
    database.publish_document_bytes(
        MEASUREMENT_DOCUMENTS,
        &measurement_document_key(&measurement_set_id, "benchmark"),
        benchmark.resolved_json()?.as_bytes(),
    )?;
    database.publish_document_bytes(
        MEASUREMENT_DOCUMENTS,
        &measurement_document_key(&measurement_set_id, "variant"),
        variant.resolved_json()?.as_bytes(),
    )?;
    database.publish_document(
        MEASUREMENT_DOCUMENTS,
        &measurement_document_key(&measurement_set_id, "schedule"),
        &schedule,
    )?;
    Ok(measurement_root)
}

fn fixed_measurement_set_id(
    benchmark: &BenchmarkManifest,
    variant: &VariantDescriptor,
    final_samples: u32,
) -> String {
    format!(
        "measure-v{MEASUREMENT_SCHEMA_VERSION}-{}-{}-s{}-n{}",
        &benchmark.source_sha256()[..12],
        &variant.source_sha256()[..12],
        benchmark.schedule_seed(),
        final_samples
    )
}

fn adaptive_measurement_set_id(
    benchmark: &BenchmarkManifest,
    variant: &VariantDescriptor,
    budget_ms: u64,
    cohort: Option<&str>,
) -> String {
    let cohort_suffix = cohort
        .map(cohort_key)
        .map(|key| format!("-c{key}"))
        .unwrap_or_default();
    format!(
        "measure-v{MEASUREMENT_SCHEMA_VERSION}-{}-{}-s{}-b{budget_ms}{cohort_suffix}",
        &benchmark.source_sha256()[..12],
        &variant.source_sha256()[..12],
        benchmark.schedule_seed(),
    )
}

fn cohort_key(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    digest.update(b"bperf-measurement-cohort-v1\0");
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())[..12].to_owned()
}

fn measurement_document_key(measurement_set_id: &str, document: &str) -> String {
    format!("{measurement_set_id}/{document}")
}

fn required_measurement_document(
    database: &Database,
    measurement_set_id: &str,
    document: &str,
) -> Result<Vec<u8>> {
    database
        .read_document_bytes(
            MEASUREMENT_DOCUMENTS,
            &measurement_document_key(measurement_set_id, document),
        )?
        .with_context(|| format!("measurement set {measurement_set_id:?} has no {document} record"))
}

pub(crate) fn write_immutable(path: &Path, content: &[u8]) -> Result<()> {
    bperf_storage::publish_immutable(path, content).with_context(|| {
        format!(
            "failed to publish immutable measurement artifact {}",
            path.display()
        )
    })
}

pub struct MeasurementSet {
    pub(crate) root: PathBuf,
    pub(crate) database: Database,
    pub(crate) benchmark: BenchmarkManifest,
    pub(crate) variant: VariantDescriptor,
    pub(crate) schedule: MeasurementSchedule,
    sampling: Option<SamplingDecision>,
    pub(crate) results: IngestedResults,
    retention: Option<ArtifactRetention>,
}

impl MeasurementSet {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_results(root, None)
    }

    pub fn open_with_results(root: &Path, results: Option<&Path>) -> Result<Self> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve measurement set {}", root.display()))?;
        let measurement_set_id = root
            .file_name()
            .and_then(|value| value.to_str())
            .context("measurement directory has a non-UTF-8 name")?;
        let collection_root = root
            .parent()
            .context("measurement set has no collection root")?;
        let database = Database::for_collection(collection_root, "measurements")?;
        let benchmark_bytes =
            required_measurement_document(&database, measurement_set_id, "benchmark")?;
        let variant_bytes =
            required_measurement_document(&database, measurement_set_id, "variant")?;
        let benchmark = BenchmarkManifest::load_resolved_bytes(&benchmark_bytes)?;
        let variant = VariantDescriptor::load_resolved_bytes(&variant_bytes)?;
        benchmark.validate_variant(&variant)?;
        let schedule: MeasurementSchedule = database
            .read_document(
                MEASUREMENT_DOCUMENTS,
                &measurement_document_key(measurement_set_id, "schedule"),
            )?
            .context("measurement set has no schedule")?;
        validate_schedule(&benchmark, &variant, measurement_set_id, &schedule)?;
        let sampling: Option<SamplingDecision> = database.read_document(
            MEASUREMENT_DOCUMENTS,
            &measurement_document_key(measurement_set_id, "sampling"),
        )?;
        let sampling = if let Some(decision) = sampling {
            decision.validate(&schedule)?;
            Some(decision)
        } else {
            None
        };
        if matches!(schedule.sampling, SamplingSchedule::Fixed) && sampling.is_some() {
            bail!("fixed measurement set cannot contain an adaptive sampling decision");
        }
        let trial_results = if let Some(path) = results {
            bperf_storage::read_json_lines(path)
                .with_context(|| format!("failed to read trial results {}", path.display()))?
        } else {
            database.read_events(MEASUREMENT_TRIALS, measurement_set_id)?
        };
        let retention = artifact_retention::load(&database, measurement_set_id)?;
        let results = ingest(
            &schedule,
            sampling.as_ref(),
            &benchmark.analysis_policy(),
            &root,
            trial_results,
            retention.is_some(),
        )?;
        if let Some(decision) = &sampling {
            validate_sampling_results(
                &schedule,
                &benchmark.analysis_policy(),
                decision,
                &results.completed,
            )?;
        }
        if let Some(retention) = &retention {
            artifact_retention::validate(&root, &schedule, &results.completed, retention)?;
        }
        Ok(Self {
            root,
            database,
            benchmark,
            variant,
            schedule,
            sampling,
            results,
            retention,
        })
    }

    pub fn measurement_set_id(&self) -> &str {
        &self.schedule.measurement_set_id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn benchmark(&self) -> &BenchmarkManifest {
        &self.benchmark
    }

    pub fn variant(&self) -> &VariantDescriptor {
        &self.variant
    }

    pub fn schedule(&self) -> &MeasurementSchedule {
        &self.schedule
    }

    pub fn preflight_run_root(&self, run_id: &str) -> PathBuf {
        self.root.join(PREFLIGHT_CAPTURE_DIRECTORY).join(run_id)
    }

    pub fn freeze_workload(&self, workload_id: &str, content: &[u8]) -> Result<()> {
        let root = self.root.join(FROZEN_WORKLOAD_DIRECTORY);
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        write_immutable(&root.join(format!("{workload_id}.json")), content)
    }

    pub fn environment_record<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        self.database.read_document(
            MEASUREMENT_DOCUMENTS,
            &measurement_document_key(self.measurement_set_id(), "environment"),
        )
    }

    pub fn write_environment_record<T: Serialize>(&self, record: &T) -> Result<()> {
        self.database.publish_document(
            MEASUREMENT_DOCUMENTS,
            &measurement_document_key(self.measurement_set_id(), "environment"),
            record,
        )
    }

    /// Persists an outcome and discards resume-only data when the trial state
    /// and retention manifest prove completion. Missing scratch directories
    /// are accepted so interrupted cleanup can be retried.
    pub fn commit_outcome<T: Serialize>(&self, summary: &T) -> Result<()> {
        let complete = !self.needs_sampling_decision() && self.pending_trials().is_empty();
        if complete && self.retention.is_none() {
            bail!("cannot commit completion before artifact retention is finalized");
        }

        let payload =
            serde_json::to_vec(summary).context("failed to encode measurement outcome")?;
        self.database.write(|transaction| {
            let current_results: Vec<TrialResult> =
                transaction.read_events(MEASUREMENT_TRIALS, self.measurement_set_id())?;
            let current_sampling: Option<SamplingDecision> = transaction.read_document(
                MEASUREMENT_DOCUMENTS,
                &measurement_document_key(self.measurement_set_id(), "sampling"),
            )?;
            let current_retention: Option<ArtifactRetention> = transaction.read_document(
                MEASUREMENT_DOCUMENTS,
                &measurement_document_key(self.measurement_set_id(), "retention"),
            )?;
            if current_results.len() != self.results.event_count
                || current_sampling.is_some() != self.sampling.is_some()
                || current_retention.is_some() != self.retention.is_some()
            {
                bail!(
                    "measurement set {} changed after it was opened; reopen it before committing an outcome",
                    self.measurement_set_id()
                );
            }
            transaction.replace_document(
                MEASUREMENT_DOCUMENTS,
                &measurement_document_key(self.measurement_set_id(), "summary"),
                &payload,
            )
        })?;

        if !complete {
            return Ok(());
        }
        for path in [
            self.root.join(PREFLIGHT_CAPTURE_DIRECTORY),
            self.root.join(FROZEN_WORKLOAD_DIRECTORY),
        ] {
            remove_measurement_directory(&self.root, &path)?;
        }
        Ok(())
    }

    pub fn benchmark_id(&self) -> &str {
        self.benchmark.benchmark_id()
    }

    pub fn subject_id(&self) -> &str {
        self.benchmark.subject_id()
    }

    pub fn variant_id(&self) -> &str {
        self.variant.id()
    }

    pub fn benchmark_sha256(&self) -> &str {
        self.benchmark.source_sha256()
    }

    pub fn variant_sha256(&self) -> &str {
        self.variant.source_sha256()
    }

    pub fn environment_fingerprint(&self) -> Option<&str> {
        self.results.environment_fingerprint.as_deref()
    }

    pub fn expected_final_trials(&self, engine: Engine) -> usize {
        self.schedule
            .trials
            .iter()
            .filter(|trial| {
                trial.phase == TrialPhase::Final
                    && trial.engine == engine
                    && self.trial_is_active(trial)
            })
            .count()
    }

    pub fn final_is_complete(&self) -> bool {
        if self.needs_sampling_decision() {
            return false;
        }
        self.schedule
            .trials
            .iter()
            .filter(|trial| trial.phase == TrialPhase::Final && self.trial_is_active(trial))
            .all(|trial| self.results.completed.contains_key(&trial.trial_id))
    }

    /// Reports whether every active trial and the representative artifact
    /// selection are durable, so the set no longer needs resume state.
    pub fn is_finalized(&self) -> bool {
        !self.needs_sampling_decision()
            && self.pending_trials().is_empty()
            && self.retention.is_some()
    }

    pub fn pending_trials(&self) -> Vec<&ScheduledTrial> {
        if self.needs_sampling_decision() {
            let warmups = self
                .schedule
                .trials
                .iter()
                .filter(|trial| {
                    trial.phase == TrialPhase::Warmup
                        && !self.results.completed.contains_key(&trial.trial_id)
                })
                .collect::<Vec<_>>();
            if !warmups.is_empty() {
                return warmups;
            }
            return self.next_pilot_trials();
        }
        self.schedule
            .trials
            .iter()
            .filter(|trial| {
                self.trial_is_active(trial) && !self.results.completed.contains_key(&trial.trial_id)
            })
            .collect()
    }

    pub fn active_trial_count(&self) -> usize {
        self.schedule
            .trials
            .iter()
            .filter(|trial| self.trial_is_active(trial))
            .count()
    }

    pub fn completed_active_trial_count(&self) -> usize {
        self.schedule
            .trials
            .iter()
            .filter(|trial| {
                self.trial_is_active(trial) && self.results.completed.contains_key(&trial.trial_id)
            })
            .count()
    }

    pub fn needs_sampling_decision(&self) -> bool {
        matches!(self.schedule.sampling, SamplingSchedule::Adaptive { .. })
            && self.sampling.is_none()
    }

    pub fn calibration_is_complete(&self) -> bool {
        let warmups_complete = self
            .schedule
            .trials
            .iter()
            .filter(|trial| trial.phase == TrialPhase::Warmup)
            .all(|trial| self.results.completed.contains_key(&trial.trial_id));
        warmups_complete
            && self
                .pilot_strata()
                .into_iter()
                .all(|(workload_id, engine)| {
                    sampling::pilot_stop_reason(
                        &self.schedule,
                        &self.benchmark.analysis_policy(),
                        workload_id,
                        engine,
                        &self.pilot_results(workload_id, engine),
                    )
                    .is_some()
                })
    }

    pub fn calibration_results(&self) -> Vec<&TrialResult> {
        self.schedule
            .trials
            .iter()
            .filter(|trial| trial.phase != TrialPhase::Final)
            .filter_map(|trial| self.results.completed.get(&trial.trial_id))
            .collect()
    }

    pub fn record_sampling_decision(&self, decision: &SamplingDecision) -> Result<()> {
        if !self.needs_sampling_decision() {
            bail!("measurement set does not need a sampling decision");
        }
        if !self.calibration_is_complete() {
            bail!("sampling cannot be locked before calibration is complete");
        }
        decision.validate(&self.schedule)?;
        validate_sampling_results(
            &self.schedule,
            &self.benchmark.analysis_policy(),
            decision,
            &self.results.completed,
        )?;
        self.database.publish_document(
            MEASUREMENT_DOCUMENTS,
            &measurement_document_key(self.measurement_set_id(), "sampling"),
            decision,
        )
    }

    pub fn sampling_decision(&self) -> Option<&SamplingDecision> {
        self.sampling.as_ref()
    }

    pub fn trial_batches(&self, trial: &ScheduledTrial) -> TrialBatchConfig {
        if trial.phase != TrialPhase::Final {
            return TrialBatchConfig::calibrating(PROFILE_BATCH_TARGET_MS, MAX_BATCH_SIZE);
        }
        self.sampling
            .as_ref()
            .and_then(|decision| decision.batch_size_for(&trial.workload_id, trial.engine))
            .map_or(TrialBatchConfig::SINGLE, TrialBatchConfig::fixed)
    }

    fn trial_is_active(&self, trial: &ScheduledTrial) -> bool {
        trial_is_active(&self.schedule, self.sampling.as_ref(), trial)
    }

    fn next_pilot_trials(&self) -> Vec<&ScheduledTrial> {
        let policy = self.benchmark.analysis_policy();
        let mut next = HashMap::new();
        for (workload_id, engine) in self.pilot_strata() {
            let pilots = self.pilot_results(workload_id, engine);
            if sampling::pilot_stop_reason(&self.schedule, &policy, workload_id, engine, &pilots)
                .is_none()
            {
                next.insert((workload_id, engine), pilots.len() as u32 + 1);
            }
        }
        self.schedule
            .trials
            .iter()
            .filter(|trial| {
                trial.phase == TrialPhase::Pilot
                    && next
                        .get(&(trial.workload_id.as_str(), trial.engine))
                        .is_some_and(|sample_index| *sample_index == trial.sample_index)
            })
            .collect()
    }

    fn pilot_strata(&self) -> Vec<(&str, Engine)> {
        let mut strata = self
            .schedule
            .trials
            .iter()
            .filter(|trial| trial.phase == TrialPhase::Pilot)
            .map(|trial| (trial.workload_id.as_str(), trial.engine))
            .collect::<Vec<_>>();
        strata.sort_unstable();
        strata.dedup();
        strata
    }

    fn pilot_results(&self, workload_id: &str, engine: Engine) -> Vec<&TrialResult> {
        let mut results = self
            .schedule
            .trials
            .iter()
            .filter(|trial| {
                trial.phase == TrialPhase::Pilot
                    && trial.workload_id == workload_id
                    && trial.engine == engine
            })
            .filter_map(|trial| self.results.completed.get(&trial.trial_id))
            .collect::<Vec<_>>();
        results.sort_by_key(|result| result.sample_index);
        results
    }

    pub fn next_attempt(&self, trial_id: &str) -> u32 {
        self.results
            .attempt_counts
            .get(trial_id)
            .copied()
            .unwrap_or_default()
            + 1
    }

    pub fn append_result(&self, result: &TrialResult) -> Result<()> {
        if self.retention.is_some() {
            bail!("artifact retention is finalized; no more trials can be appended");
        }
        if result.measurement_set_id != self.measurement_set_id() {
            bail!("trial result belongs to a different measurement set");
        }
        if self.results.completed.contains_key(&result.trial_id) {
            bail!(
                "trial {} already has a valid terminal result",
                result.trial_id
            );
        }
        if let Some(expected) = &self.results.environment_fingerprint
            && expected != &result.environment_fingerprint
        {
            bail!("trial result environment differs from the measurement set");
        }
        let scheduled = self
            .schedule
            .trials
            .iter()
            .find(|trial| trial.trial_id == result.trial_id)
            .with_context(|| format!("unknown scheduled trial {}", result.trial_id))?;
        if !self
            .pending_trials()
            .iter()
            .any(|trial| trial.trial_id == scheduled.trial_id)
        {
            bail!("trial {} is not currently pending", result.trial_id);
        }
        validate_trial_identity(scheduled, result)?;
        validate_result(result, &self.benchmark.analysis_policy())?;
        artifact_retention::validate_result(&self.root, result, false)?;
        let expected_attempt = self.next_attempt(&result.trial_id);
        if result.attempt != expected_attempt {
            bail!(
                "{} expected attempt {}, received {}",
                result.trial_id,
                expected_attempt,
                result.attempt
            );
        }

        let payload = serde_json::to_vec(result).context("failed to encode trial result")?;
        self.database
            .write(|transaction| {
                let current: Vec<TrialResult> = transaction
                    .read_events(MEASUREMENT_TRIALS, self.measurement_set_id())?;
                let current_sampling: Option<SamplingDecision> = transaction.read_document(
                    MEASUREMENT_DOCUMENTS,
                    &measurement_document_key(self.measurement_set_id(), "sampling"),
                )?;
                if let Some(decision) = &current_sampling {
                    decision.validate(&self.schedule)?;
                }
                if !trial_is_active(&self.schedule, current_sampling.as_ref(), scheduled) {
                    bail!("trial {} is outside the active sampling prefix", result.trial_id);
                }
                if current
                    .iter()
                    .any(|attempt| attempt.trial_id == result.trial_id && attempt.valid)
                {
                    bail!(
                        "trial {} already has a valid terminal result",
                        result.trial_id
                    );
                }
                if let Some(expected) = current
                    .first()
                    .map(|attempt| &attempt.environment_fingerprint)
                    && expected != &result.environment_fingerprint
                {
                    bail!("trial result environment differs from the measurement set");
                }
                let prior_attempts = current
                    .iter()
                    .filter(|attempt| attempt.trial_id == result.trial_id)
                    .count();
                let expected_attempt = u32::try_from(prior_attempts)
                    .context("trial attempt count does not fit in u32")?
                    .checked_add(1)
                    .context("trial attempt count overflow")?;
                if result.attempt != expected_attempt {
                    bail!(
                        "{} expected attempt {}, received {}",
                        result.trial_id,
                        expected_attempt,
                        result.attempt
                    );
                }
                transaction.append_event_if_unchanged(
                    MEASUREMENT_TRIALS,
                    self.measurement_set_id(),
                    current.len(),
                    &payload,
                )?;
                Ok(())
            })
            .with_context(|| {
                format!(
                    "measurement set {} changed while trial {} was running; reopen it before retrying",
                    self.measurement_set_id(),
                    result.trial_id
                )
            })
    }

    pub fn final_results(&self, engine: Engine) -> Vec<&TrialResult> {
        self.schedule
            .trials
            .iter()
            .filter(|trial| {
                trial.phase == TrialPhase::Final
                    && trial.engine == engine
                    && self.trial_is_active(trial)
            })
            .filter_map(|trial| self.results.completed.get(&trial.trial_id))
            .collect()
    }

    /// Returns only native payloads retained by a finalized representative
    /// selection. Descriptors for discarded trial payloads never cross this
    /// boundary even though they remain in immutable trial evidence.
    pub fn retained_artifacts(&self) -> Vec<(Engine, &ArtifactEvidence)> {
        self.retention
            .iter()
            .flat_map(ArtifactRetention::artifacts)
            .collect()
    }

    pub fn invalid_attempts(&self, engine: Engine) -> usize {
        self.results
            .invalid_attempts
            .get(&engine)
            .copied()
            .unwrap_or_default()
    }
}

fn trial_is_active(
    schedule: &MeasurementSchedule,
    sampling: Option<&SamplingDecision>,
    trial: &ScheduledTrial,
) -> bool {
    match (&schedule.sampling, sampling) {
        (SamplingSchedule::Fixed, None) => true,
        (SamplingSchedule::Fixed, Some(_)) => false,
        (SamplingSchedule::Adaptive { .. }, None) => trial.phase != TrialPhase::Final,
        (SamplingSchedule::Adaptive { .. }, Some(decision)) => match trial.phase {
            TrialPhase::Warmup => true,
            TrialPhase::Pilot => decision
                .pilot_samples_for(&trial.workload_id, trial.engine)
                .is_some_and(|samples| trial.sample_index <= samples),
            TrialPhase::Final => decision
                .final_samples_for(&trial.workload_id, trial.engine)
                .is_some_and(|samples| trial.sample_index <= samples),
        },
    }
}

fn remove_measurement_directory(root: &Path, path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "measurement cleanup refuses non-directory {}",
            path.display()
        );
    }
    let resolved =
        fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))?;
    if resolved.parent() != Some(root) {
        bail!(
            "measurement cleanup path escaped measurement set {}",
            root.display()
        );
    }
    fs::remove_dir_all(&resolved)
        .with_context(|| format!("failed to remove {}", resolved.display()))
}

fn validate_sampling_results(
    schedule: &MeasurementSchedule,
    policy: &AnalysisPolicy,
    decision: &SamplingDecision,
    completed: &HashMap<String, TrialResult>,
) -> Result<()> {
    for trial in &schedule.trials {
        match trial.phase {
            TrialPhase::Warmup if !completed.contains_key(&trial.trial_id) => {
                bail!(
                    "sampling decision was recorded before warmup {} completed",
                    trial.trial_id
                );
            }
            TrialPhase::Pilot => {
                let selected = decision
                    .pilot_samples_for(&trial.workload_id, trial.engine)
                    .with_context(|| {
                        format!(
                            "sampling decision has no pilot count for {}/{}",
                            trial.workload_id, trial.engine
                        )
                    })?;
                let present = completed.contains_key(&trial.trial_id);
                if present != (trial.sample_index <= selected) {
                    bail!(
                        "pilot evidence for {}/{} does not match its locked prefix",
                        trial.workload_id,
                        trial.engine
                    );
                }
            }
            _ => {}
        }
    }
    for stratum in &decision.strata {
        let mut pilots = schedule
            .trials
            .iter()
            .filter(|trial| {
                trial.phase == TrialPhase::Pilot
                    && trial.workload_id == stratum.workload_id
                    && trial.engine == stratum.engine
                    && trial.sample_index <= stratum.pilot_samples
            })
            .filter_map(|trial| completed.get(&trial.trial_id))
            .collect::<Vec<_>>();
        pilots.sort_by_key(|result| result.sample_index);
        if sampling::pilot_stop_reason(
            schedule,
            policy,
            &stratum.workload_id,
            stratum.engine,
            &pilots,
        ) != Some(stratum.pilot_stop_reason)
        {
            bail!(
                "pilot stopping evidence changed for {}/{}",
                stratum.workload_id,
                stratum.engine
            );
        }
    }
    Ok(())
}

fn validate_schedule(
    benchmark: &BenchmarkManifest,
    variant: &VariantDescriptor,
    measurement_set_id: &str,
    schedule: &MeasurementSchedule,
) -> Result<()> {
    if schedule.schema_version != MEASUREMENT_SCHEMA_VERSION {
        bail!(
            "schedule uses unsupported schema {}",
            schedule.schema_version
        );
    }
    if schedule.benchmark_id != benchmark.benchmark_id()
        || schedule.subject_id != benchmark.subject_id()
        || schedule.benchmark_sha256 != benchmark.source_sha256()
    {
        bail!("schedule and resolved benchmark identities do not match");
    }
    if schedule.variant_id != variant.id() || schedule.variant_sha256 != variant.source_sha256() {
        bail!("schedule and resolved variant identities do not match");
    }
    if schedule
        .cohort
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        bail!("measurement cohort must not be empty");
    }
    let expected = match schedule.sampling {
        SamplingSchedule::Fixed => {
            let final_samples = benchmark.resolve_final_samples(Some(schedule.final_samples))?;
            let expected_id = fixed_measurement_set_id(benchmark, variant, final_samples);
            MeasurementSchedule::build(benchmark, variant, expected_id, final_samples)
        }
        SamplingSchedule::Adaptive {
            budget_ms,
            min_final_samples,
        } => {
            let (expected_minimum, expected_maximum) = benchmark.adaptive_final_sample_range()?;
            if budget_ms == 0
                || min_final_samples != expected_minimum
                || schedule.final_samples != expected_maximum
            {
                bail!("adaptive schedule does not match the resolved benchmark policy");
            }
            let expected_id = adaptive_measurement_set_id(
                benchmark,
                variant,
                budget_ms,
                schedule.cohort.as_deref(),
            );
            MeasurementSchedule::build_adaptive(
                benchmark,
                variant,
                expected_id,
                budget_ms,
                expected_minimum,
                expected_maximum,
                schedule.cohort.clone(),
            )
        }
    };
    if expected.measurement_set_id != measurement_set_id {
        bail!("measurement directory does not match its deterministic schedule identity");
    }
    if schedule != &expected {
        bail!("schedule does not match the deterministic benchmark capture contract");
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialResult {
    pub schema_version: u32,
    pub measurement_set_id: String,
    pub trial_id: String,
    pub attempt: u32,
    pub workload_id: String,
    pub engine: Engine,
    pub phase: TrialPhase,
    pub sample_index: u32,
    pub environment_fingerprint: String,
    pub valid: bool,
    pub success: bool,
    #[serde(default)]
    pub failure_category: Option<String>,
    #[serde(default)]
    pub failure_detail: Option<String>,
    #[serde(default)]
    pub invalidation_reason: Option<String>,
    #[serde(default)]
    pub metrics: BTreeMap<String, f64>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactEvidence>,
}

pub(crate) struct IngestedResults {
    pub(crate) completed: HashMap<String, TrialResult>,
    invalid_attempts: HashMap<Engine, usize>,
    environment_fingerprint: Option<String>,
    attempt_counts: HashMap<String, u32>,
    event_count: usize,
}

fn ingest(
    schedule: &MeasurementSchedule,
    sampling: Option<&SamplingDecision>,
    policy: &AnalysisPolicy,
    measurement_root: &Path,
    results: Vec<TrialResult>,
    retention_finalized: bool,
) -> Result<IngestedResults> {
    let event_count = results.len();
    let expected: HashMap<_, _> = schedule
        .trials
        .iter()
        .map(|trial| (trial.trial_id.as_str(), trial))
        .collect();
    let mut attempts: HashMap<String, Vec<TrialResult>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut environment_fingerprints = HashSet::new();

    for result in results {
        if result.schema_version != MEASUREMENT_SCHEMA_VERSION {
            bail!(
                "trial result {} uses unsupported schema {}",
                result.trial_id,
                result.schema_version
            );
        }
        if result.measurement_set_id != schedule.measurement_set_id {
            bail!(
                "trial {} belongs to measurement set {:?}, expected {:?}",
                result.trial_id,
                result.measurement_set_id,
                schedule.measurement_set_id
            );
        }
        if result.attempt == 0 {
            bail!("trial result {} has attempt 0", result.trial_id);
        }
        let expected_trial = expected
            .get(result.trial_id.as_str())
            .with_context(|| format!("unknown scheduled trial {}", result.trial_id))?;
        if !trial_is_active(schedule, sampling, expected_trial) {
            bail!(
                "trial {} is outside the active sampling prefix",
                result.trial_id
            );
        }
        validate_trial_identity(expected_trial, &result)?;
        if !seen.insert((result.trial_id.clone(), result.attempt)) {
            bail!(
                "duplicate result for {} attempt {}",
                result.trial_id,
                result.attempt
            );
        }
        validate_result(&result, policy)?;
        artifact_retention::validate_result(measurement_root, &result, retention_finalized)?;
        environment_fingerprints.insert(result.environment_fingerprint.clone());
        attempts
            .entry(result.trial_id.clone())
            .or_default()
            .push(result);
    }

    if environment_fingerprints.len() > 1 {
        bail!("measurement set contains multiple environment fingerprints");
    }
    let environment_fingerprint = environment_fingerprints.into_iter().next();
    let attempt_counts = attempts
        .iter()
        .map(|(trial_id, values)| (trial_id.clone(), values.len() as u32))
        .collect();
    let mut completed = HashMap::new();
    let mut invalid_attempts: HashMap<Engine, usize> = HashMap::new();
    for (trial_id, trial_attempts) in &mut attempts {
        trial_attempts.sort_by_key(|attempt| attempt.attempt);
        for (index, attempt) in trial_attempts.iter().enumerate() {
            let expected_attempt = index as u32 + 1;
            if attempt.attempt != expected_attempt {
                bail!(
                    "{trial_id} attempts are not contiguous: expected {expected_attempt}, found {}",
                    attempt.attempt
                );
            }
            if !attempt.valid {
                *invalid_attempts.entry(attempt.engine).or_default() += 1;
            } else {
                if index + 1 != trial_attempts.len() {
                    bail!("{trial_id} has attempts after its valid terminal result");
                }
                completed.insert(trial_id.clone(), attempt.clone());
            }
        }
    }

    Ok(IngestedResults {
        completed,
        invalid_attempts,
        environment_fingerprint,
        attempt_counts,
        event_count,
    })
}

fn validate_trial_identity(expected: &ScheduledTrial, result: &TrialResult) -> Result<()> {
    if result.workload_id != expected.workload_id
        || result.engine != expected.engine
        || result.phase != expected.phase
        || result.sample_index != expected.sample_index
    {
        bail!(
            "trial {} metadata does not match its immutable schedule",
            result.trial_id
        );
    }
    Ok(())
}

fn validate_result(result: &TrialResult, policy: &AnalysisPolicy) -> Result<()> {
    if result.environment_fingerprint.trim().is_empty() {
        bail!(
            "trial {} has an empty environment_fingerprint",
            result.trial_id
        );
    }
    match (result.valid, result.invalidation_reason.as_deref()) {
        (false, Some(reason)) if !reason.trim().is_empty() => {}
        (false, _) => bail!(
            "invalid trial {} must include invalidation_reason",
            result.trial_id
        ),
        (true, Some(_)) => bail!(
            "valid trial {} cannot include invalidation_reason",
            result.trial_id
        ),
        (true, None) => {}
    }
    match (result.success, result.failure_category.as_deref()) {
        (false, Some(category)) if !category.trim().is_empty() => {}
        (false, _) if result.valid => bail!(
            "failed valid trial {} must include failure_category",
            result.trial_id
        ),
        (true, Some(_)) => bail!(
            "successful trial {} cannot include failure_category",
            result.trial_id
        ),
        _ => {}
    }
    if result.success && result.failure_detail.is_some() {
        bail!(
            "successful trial {} cannot include failure_detail",
            result.trial_id
        );
    }
    for (metric, value) in &result.metrics {
        if !value.is_finite() || *value < 0.0 {
            bail!(
                "trial {} metric {metric:?} must be finite and non-negative",
                result.trial_id
            );
        }
    }
    if result.valid && result.success {
        for metric in &policy.primary_metrics {
            let value = result.metrics.get(&metric.name).with_context(|| {
                format!(
                    "successful trial {} has no primary metric {:?}",
                    result.trial_id, metric.name
                )
            })?;
            if *value <= 0.0 {
                bail!(
                    "successful trial {} metric {:?} must be positive for ratio analysis",
                    result.trial_id,
                    metric.name
                );
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct ValidationSummary<'a> {
    schema_version: u32,
    status: &'static str,
    benchmark_path: String,
    benchmark_sha256: &'a str,
    benchmark_id: &'a str,
    subject_id: &'a str,
    variant: Option<VariantSummary<'a>>,
    workloads: Vec<&'a str>,
    engines: &'a [Engine],
}

impl<'a> ValidationSummary<'a> {
    fn new(benchmark: &'a BenchmarkManifest, variant: Option<&'a VariantDescriptor>) -> Self {
        Self {
            schema_version: 1,
            status: "valid",
            benchmark_path: benchmark.source_path().to_string_lossy().into_owned(),
            benchmark_sha256: benchmark.source_sha256(),
            benchmark_id: benchmark.benchmark_id(),
            subject_id: benchmark.subject_id(),
            variant: variant.map(|variant| VariantSummary {
                id: variant.id(),
                path: variant.source_path().to_string_lossy().into_owned(),
                sha256: variant.source_sha256(),
            }),
            workloads: benchmark.workload_ids().collect(),
            engines: benchmark.engines(),
        }
    }
}

#[derive(Serialize)]
struct VariantSummary<'a> {
    id: &'a str,
    path: String,
    sha256: &'a str,
}

#[derive(Serialize)]
struct PlanSummary<'a> {
    schema_version: u32,
    measurement_set_id: String,
    benchmark_id: &'a str,
    subject_id: &'a str,
    variant_id: &'a str,
    benchmark_sha256: &'a str,
    variant_sha256: &'a str,
    measurement_root: std::borrow::Cow<'a, str>,
    trial_count: usize,
    final_trial_count: usize,
    final_samples: u32,
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::*;
    use crate::{
        retention as artifact_retention,
        sampling::{self, BATCH_SIZE_METRIC, CAPTURE_ELAPSED_METRIC, TRIAL_ELAPSED_METRIC},
    };
    use bperf_browser::lab::ArtifactKind;

    #[test]
    fn immutable_write_is_idempotent_but_never_overwrites() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("schedule.json");
        write_immutable(&path, b"first").unwrap();
        write_immutable(&path, b"first").unwrap();
        let error = write_immutable(&path, b"second").unwrap_err();
        assert!(format!("{error:#}").contains("immutable file collision"));
    }

    #[test]
    fn measurement_reopen_reads_transactionally_committed_trial_events() {
        let directory = tempdir().unwrap();
        let root = prepare(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            Some(20),
            directory.path(),
        )
        .unwrap();
        let measurement = MeasurementSet::open(&root).unwrap();
        append_pending(&measurement);

        let reopened = MeasurementSet::open(&root).unwrap();
        assert!(reopened.final_is_complete());
        assert!(!root.join("trials.jsonl").exists());
    }

    #[test]
    fn stored_schedules_cannot_omit_requested_engine_trials() {
        let benchmark = BenchmarkManifest::load(&example("browser-benchmark.yaml")).unwrap();
        let variant = VariantDescriptor::load(&example("browser-variant-baseline.yaml")).unwrap();
        let final_samples = benchmark.resolve_final_samples(Some(20)).unwrap();
        let measurement_set_id = fixed_measurement_set_id(&benchmark, &variant, final_samples);
        let mut schedule = MeasurementSchedule::build(
            &benchmark,
            &variant,
            measurement_set_id.clone(),
            final_samples,
        );
        validate_schedule(&benchmark, &variant, &measurement_set_id, &schedule).unwrap();

        schedule
            .trials
            .retain(|trial| trial.engine != Engine::Webkit);
        let error =
            validate_schedule(&benchmark, &variant, &measurement_set_id, &schedule).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("deterministic benchmark capture contract"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn stale_measurement_handle_cannot_duplicate_a_trial_attempt() {
        let directory = tempdir().unwrap();
        let root = prepare(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            Some(20),
            directory.path(),
        )
        .unwrap();
        let first = MeasurementSet::open(&root).unwrap();
        let stale = MeasurementSet::open(&root).unwrap();
        let trial = first.pending_trials()[0];
        let result = synthetic_result(&first, trial, 100.0);
        first.append_result(&result).unwrap();

        let error = stale.append_result(&result).unwrap_err();
        assert!(
            format!("{error:#}").contains("changed while trial"),
            "unexpected error: {error:#}"
        );
        let reopened = MeasurementSet::open(&root).unwrap();
        assert_eq!(
            reopened
                .database
                .read_events::<TrialResult>(MEASUREMENT_TRIALS, reopened.measurement_set_id())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(reopened.next_attempt(&trial.trial_id), 2);
    }

    #[test]
    fn stale_measurement_handle_cannot_overwrite_a_completed_outcome() {
        let directory = tempdir().unwrap();
        let root = prepare(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            Some(20),
            directory.path(),
        )
        .unwrap();
        let stale = MeasurementSet::open(&root).unwrap();
        append_pending(&stale);

        let completed = MeasurementSet::open(&root).unwrap();
        artifact_retention::finalize(&completed).unwrap().unwrap();
        let retained = MeasurementSet::open(&root).unwrap();
        let completed_summary = serde_json::json!({"status": "complete"});
        retained.commit_outcome(&completed_summary).unwrap();

        let error = stale
            .commit_outcome(&serde_json::json!({"status": "open"}))
            .unwrap_err();
        assert!(
            error.to_string().contains("changed after it was opened"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            retained
                .database
                .read_document::<serde_json::Value>(
                    MEASUREMENT_DOCUMENTS,
                    &measurement_document_key(retained.measurement_set_id(), "summary"),
                )
                .unwrap()
                .unwrap(),
            completed_summary
        );
    }

    #[test]
    fn adaptive_resume_activates_only_the_locked_final_prefixes() {
        let directory = tempdir().unwrap();
        let root = prepare_adaptive(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            "1h".parse().unwrap(),
            None,
            directory.path(),
        )
        .unwrap();
        let mut calibrated = MeasurementSet::open(&root).unwrap();
        assert!(calibrated.needs_sampling_decision());
        assert_eq!(calibrated.pending_trials().len(), 9);
        while !calibrated.calibration_is_complete() {
            append_pending(&calibrated);
            calibrated = MeasurementSet::open(&root).unwrap();
        }
        assert!(calibrated.calibration_is_complete());
        let decision = sampling::decide(
            &calibrated.schedule,
            &calibrated.benchmark.analysis_policy(),
            &calibrated.calibration_results(),
        )
        .unwrap();

        let mut unsupported_prefix = decision.clone();
        unsupported_prefix.strata[0].pilot_samples += 1;
        for metric in &mut unsupported_prefix.strata[0].metrics {
            metric.observations += 1;
        }
        let error = calibrated
            .record_sampling_decision(&unsupported_prefix)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its locked prefix")
        );
        assert!(
            calibrated
                .database
                .read_document_bytes(
                    MEASUREMENT_DOCUMENTS,
                    &measurement_document_key(calibrated.measurement_set_id(), "sampling"),
                )
                .unwrap()
                .is_none()
        );

        calibrated.record_sampling_decision(&decision).unwrap();

        let selected = MeasurementSet::open(&root).unwrap();
        assert_eq!(decision.selected_final_trials, 60);
        assert!(decision.strata.iter().all(|stratum| {
            stratum.pilot_samples == 5
                && stratum.pilot_stop_reason == sampling::PilotStopReason::Stable
        }));
        assert_eq!(selected.pending_trials().len(), 60);
        assert_eq!(selected.active_trial_count(), 84);
        assert_eq!(selected.schedule.trials.len(), 339);
        append_pending(&selected);

        let completed = MeasurementSet::open(&root).unwrap();
        assert!(completed.final_is_complete());
        assert!(completed.pending_trials().is_empty());
        assert_eq!(completed.completed_active_trial_count(), 84);
        assert_eq!(
            completed
                .database
                .read_events::<TrialResult>(MEASUREMENT_TRIALS, completed.measurement_set_id())
                .unwrap()
                .len(),
            84
        );
    }

    #[test]
    fn adaptive_trials_must_stay_inside_the_active_prefix() {
        let directory = tempdir().unwrap();
        let root = prepare_adaptive(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            "1h".parse().unwrap(),
            None,
            directory.path(),
        )
        .unwrap();
        let measurement = MeasurementSet::open(&root).unwrap();
        let final_trial = measurement
            .schedule
            .trials
            .iter()
            .find(|trial| trial.phase == TrialPhase::Final)
            .unwrap();
        let result = synthetic_result(&measurement, final_trial, 100.0);

        let error = measurement.append_result(&result).unwrap_err();
        assert!(
            error.to_string().contains("is not currently pending"),
            "unexpected error: {error:#}"
        );
        assert!(
            measurement
                .database
                .read_events::<TrialResult>(MEASUREMENT_TRIALS, measurement.measurement_set_id())
                .unwrap()
                .is_empty()
        );

        measurement
            .database
            .append_event(
                MEASUREMENT_TRIALS,
                measurement.measurement_set_id(),
                &result,
            )
            .unwrap();
        let error = MeasurementSet::open(&root)
            .err()
            .expect("out-of-prefix evidence must be rejected");
        assert!(
            format!("{error:#}").contains("outside the active sampling prefix"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn stable_pilot_strata_stop_while_noisy_strata_continue() {
        let directory = tempdir().unwrap();
        let root = prepare_adaptive(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            "1h".parse().unwrap(),
            None,
            directory.path(),
        )
        .unwrap();
        let mut measurement = MeasurementSet::open(&root).unwrap();
        append_pending(&measurement);
        measurement = MeasurementSet::open(&root).unwrap();

        for _ in 0..5 {
            append_pending_with(&measurement, |trial| {
                if trial.engine == Engine::Firefox && trial.sample_index == 5 {
                    400.0
                } else {
                    100.0
                }
            });
            measurement = MeasurementSet::open(&root).unwrap();
        }

        assert_eq!(
            measurement
                .pending_trials()
                .iter()
                .map(|trial| trial.trial_id.as_str())
                .collect::<Vec<_>>(),
            vec!["pilot-checkout-flow-firefox-0006"]
        );
    }

    #[test]
    fn confirmation_cohorts_create_independent_measurement_sets() {
        let directory = tempdir().unwrap();
        let benchmark = example("browser-benchmark.yaml");
        let variant = example("browser-variant-baseline.yaml");
        let budget = "1m".parse().unwrap();
        let regular =
            prepare_adaptive(&benchmark, &variant, budget, None, directory.path()).unwrap();
        let confirmation = prepare_adaptive(
            &benchmark,
            &variant,
            budget,
            Some("confirmation:cycle-1"),
            directory.path(),
        )
        .unwrap();

        assert_ne!(regular, confirmation);
        let confirmation = MeasurementSet::open(&confirmation).unwrap();
        assert_eq!(
            confirmation.schedule.cohort.as_deref(),
            Some("confirmation:cycle-1")
        );
    }

    #[test]
    fn execution_scratch_survives_until_retained_evidence_is_complete() {
        let directory = tempdir().unwrap();
        let root = prepare(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            Some(20),
            directory.path(),
        )
        .unwrap();
        let measurement = MeasurementSet::open(&root).unwrap();
        let preflight_marker = measurement.preflight_run_root("test").join("capture.bin");
        let workload_marker = root.join("workloads").join("checkout.json");
        fs::create_dir_all(preflight_marker.parent().unwrap()).unwrap();
        fs::write(&preflight_marker, b"capture").unwrap();
        measurement
            .freeze_workload("checkout", b"workload")
            .unwrap();

        measurement
            .commit_outcome(&serde_json::json!({"status": "open"}))
            .unwrap();
        assert!(preflight_marker.is_file());
        assert!(workload_marker.is_file());

        append_pending(&measurement);
        let completed = MeasurementSet::open(&root).unwrap();
        assert!(!completed.is_finalized());
        let retention = artifact_retention::finalize(&completed).unwrap().unwrap();
        let retained = MeasurementSet::open(&root).unwrap();
        assert!(retained.is_finalized());
        assert_eq!(
            retained.retained_artifacts().len(),
            retention.retained_artifacts
        );
        let summary = serde_json::json!({"status": "complete"});
        retained.commit_outcome(&summary).unwrap();
        retained.commit_outcome(&summary).unwrap();

        assert!(!root.join("preflight").exists());
        assert!(!root.join("workloads").exists());
        assert!(!root.join("trials.jsonl").exists());
        assert!(!root.join("artifact-retention.json").exists());
        assert!(
            retained
                .database
                .read_document_bytes(
                    MEASUREMENT_DOCUMENTS,
                    &measurement_document_key(retained.measurement_set_id(), "retention"),
                )
                .unwrap()
                .is_some()
        );
        assert_eq!(
            retained
                .database
                .read_document::<serde_json::Value>(
                    MEASUREMENT_DOCUMENTS,
                    &measurement_document_key(retained.measurement_set_id(), "summary"),
                )
                .unwrap()
                .unwrap(),
            summary
        );
        MeasurementSet::open(&root).unwrap();
    }

    fn example(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("examples")
            .join(name)
    }

    fn append_pending(measurement: &MeasurementSet) {
        append_pending_with(measurement, |_| 100.0);
    }

    fn append_pending_with(
        measurement: &MeasurementSet,
        metric_value: impl Fn(&ScheduledTrial) -> f64,
    ) {
        for trial in measurement.pending_trials() {
            let metric_value = metric_value(trial);
            measurement
                .append_result(&synthetic_result(measurement, trial, metric_value))
                .unwrap();
        }
    }

    fn synthetic_result(
        measurement: &MeasurementSet,
        trial: &ScheduledTrial,
        metric_value: f64,
    ) -> TrialResult {
        TrialResult {
            schema_version: MEASUREMENT_SCHEMA_VERSION,
            measurement_set_id: measurement.measurement_set_id().to_owned(),
            trial_id: trial.trial_id.clone(),
            attempt: measurement.next_attempt(&trial.trial_id),
            workload_id: trial.workload_id.clone(),
            engine: trial.engine,
            phase: trial.phase,
            sample_index: trial.sample_index,
            environment_fingerprint: "test-environment".into(),
            valid: true,
            success: true,
            failure_category: None,
            failure_detail: None,
            invalidation_reason: None,
            metrics: BTreeMap::from([
                ("workload.wall_ms".into(), metric_value),
                ("variant.call_wall_ms".into(), metric_value / 2.0),
                ("browser.cpu_profile.active_ms".into(), metric_value),
                ("browser.js_heap.live_bytes".into(), metric_value),
                (CAPTURE_ELAPSED_METRIC.into(), 30.0),
                (BATCH_SIZE_METRIC.into(), 1.0),
                (TRIAL_ELAPSED_METRIC.into(), 30.0),
            ]),
            artifacts: synthetic_artifacts(measurement.root(), &trial.trial_id, trial.engine),
        }
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
            let relative = PathBuf::from("synthetic")
                .join(trial_id)
                .join(format!("{name}.txt"));
            let path = root.join(&relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes = format!("{trial_id}-{name}").into_bytes();
            fs::write(&path, &bytes).unwrap();
            ArtifactEvidence {
                capture_scope: match engine {
                    Engine::Firefox => "browser-context",
                    Engine::Chromium | Engine::Webkit => "page",
                }
                .to_owned(),
                kind,
                path: relative.to_string_lossy().replace('\\', "/"),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                format: "synthetic".into(),
            }
        })
        .collect()
    }
}
