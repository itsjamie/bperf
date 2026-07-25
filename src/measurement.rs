//! Measurement-set preparation, loading, and result validation.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    MEASUREMENT_SCHEMA_VERSION,
    artifact_retention::{self, ArtifactRetention},
    browser_lab::{ArtifactEvidence, Engine, TrialBatchConfig},
    manifest::{AnalysisPolicy, BenchmarkManifest, VariantDescriptor},
    sampling::{self, MAX_BATCH_SIZE, PROFILE_BATCH_TARGET_MS, RunBudget, SamplingDecision},
    schedule::{MeasurementSchedule, SamplingSchedule, ScheduledTrial, TrialPhase},
};

const PREFLIGHT_CAPTURE_DIRECTORY: &str = "preflight";
const FROZEN_WORKLOAD_DIRECTORY: &str = "workloads";

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

pub(crate) fn prepare(
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

pub(crate) fn prepare_adaptive(
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
            let measurement_set_id = format!(
                "measure-v{MEASUREMENT_SCHEMA_VERSION}-{}-{}-s{}-n{}",
                &benchmark.source_sha256()[..12],
                &variant.source_sha256()[..12],
                benchmark.schedule_seed(),
                final_samples
            );
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
            let cohort_suffix = cohort
                .as_deref()
                .map(cohort_key)
                .map(|key| format!("-c{key}"))
                .unwrap_or_default();
            let measurement_set_id = format!(
                "measure-v{MEASUREMENT_SCHEMA_VERSION}-{}-{}-s{}-b{}{}",
                &benchmark.source_sha256()[..12],
                &variant.source_sha256()[..12],
                benchmark.schedule_seed(),
                budget.milliseconds(),
                cohort_suffix
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

    write_immutable(
        &measurement_root.join("benchmark.resolved.json"),
        format!("{}\n", benchmark.resolved_json()?).as_bytes(),
    )?;
    write_immutable(
        &measurement_root.join("variant.resolved.json"),
        format!("{}\n", variant.resolved_json()?).as_bytes(),
    )?;
    write_immutable(
        &measurement_root.join("schedule.json"),
        format!("{}\n", serde_json::to_string_pretty(&schedule)?).as_bytes(),
    )?;
    Ok(measurement_root)
}

fn cohort_key(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    digest.update(b"bperf-measurement-cohort-v1\0");
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())[..12].to_owned()
}

pub(crate) fn write_immutable(path: &Path, content: &[u8]) -> Result<()> {
    if path.exists() {
        let existing =
            fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        if existing == content {
            return Ok(());
        }
        bail!(
            "refusing to overwrite immutable measurement artifact {}",
            path.display()
        );
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush {}", path.display()))
}

pub struct MeasurementSet {
    pub(crate) root: PathBuf,
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
        let benchmark = BenchmarkManifest::load_resolved(&root.join("benchmark.resolved.json"))?;
        let variant = VariantDescriptor::load_resolved(&root.join("variant.resolved.json"))?;
        benchmark.validate_variant(&variant)?;
        let schedule: MeasurementSchedule = serde_json::from_slice(
            &fs::read(root.join("schedule.json")).context("failed to read schedule.json")?,
        )
        .context("invalid schedule.json")?;
        validate_schedule(&benchmark, &variant, &schedule)?;
        let sampling_path = root.join("sampling.json");
        let sampling = if sampling_path.exists() {
            let decision: SamplingDecision = serde_json::from_slice(
                &fs::read(&sampling_path)
                    .with_context(|| format!("failed to read {}", sampling_path.display()))?,
            )
            .with_context(|| format!("invalid {}", sampling_path.display()))?;
            decision.validate(&schedule)?;
            Some(decision)
        } else {
            None
        };
        if matches!(schedule.sampling, SamplingSchedule::Fixed) && sampling.is_some() {
            bail!("fixed measurement set cannot contain sampling.json");
        }
        let results_path = results
            .map(Path::to_owned)
            .unwrap_or_else(|| root.join("trials.jsonl"));
        let retention = artifact_retention::load(&root)?;
        let results = ingest(
            &schedule,
            &benchmark.analysis_policy(),
            &root,
            &results_path,
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

    pub(crate) fn preflight_capture_root(&self) -> PathBuf {
        self.root.join(PREFLIGHT_CAPTURE_DIRECTORY)
    }

    pub(crate) fn frozen_workload_root(&self) -> PathBuf {
        self.root.join(FROZEN_WORKLOAD_DIRECTORY)
    }

    /// Persists an outcome and discards resume-only data when the trial state
    /// and retention manifest prove completion. Missing scratch directories
    /// are accepted so interrupted cleanup can be retried.
    pub(crate) fn commit_outcome<T: Serialize>(&self, summary: &T) -> Result<()> {
        let complete = !self.needs_sampling_decision() && self.pending_trials().is_empty();
        if complete && self.retention.is_none() {
            bail!("cannot commit completion before artifact retention is finalized");
        }

        let encoded = serde_json::to_string_pretty(summary)?;
        let summary_path = self.root.join("summary.json");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&summary_path)
            .with_context(|| format!("failed to open {}", summary_path.display()))?;
        file.write_all(format!("{encoded}\n").as_bytes())
            .with_context(|| format!("failed to write {}", summary_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to flush {}", summary_path.display()))?;

        if !complete {
            return Ok(());
        }
        for path in [self.preflight_capture_root(), self.frozen_workload_root()] {
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

    pub(crate) fn pending_trials(&self) -> Vec<&ScheduledTrial> {
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

    pub(crate) fn active_trial_count(&self) -> usize {
        self.schedule
            .trials
            .iter()
            .filter(|trial| self.trial_is_active(trial))
            .count()
    }

    pub(crate) fn completed_active_trial_count(&self) -> usize {
        self.schedule
            .trials
            .iter()
            .filter(|trial| {
                self.trial_is_active(trial) && self.results.completed.contains_key(&trial.trial_id)
            })
            .count()
    }

    pub(crate) fn needs_sampling_decision(&self) -> bool {
        matches!(self.schedule.sampling, SamplingSchedule::Adaptive { .. })
            && self.sampling.is_none()
    }

    pub(crate) fn calibration_is_complete(&self) -> bool {
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

    pub(crate) fn calibration_results(&self) -> Vec<&TrialResult> {
        self.schedule
            .trials
            .iter()
            .filter(|trial| trial.phase != TrialPhase::Final)
            .filter_map(|trial| self.results.completed.get(&trial.trial_id))
            .collect()
    }

    pub(crate) fn record_sampling_decision(&self, decision: &SamplingDecision) -> Result<()> {
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
        write_immutable(
            &self.root.join("sampling.json"),
            format!("{}\n", serde_json::to_string_pretty(decision)?).as_bytes(),
        )
    }

    pub(crate) fn sampling_decision(&self) -> Option<&SamplingDecision> {
        self.sampling.as_ref()
    }

    pub(crate) fn trial_batches(&self, trial: &ScheduledTrial) -> TrialBatchConfig {
        if trial.phase != TrialPhase::Final {
            return TrialBatchConfig::calibrating(PROFILE_BATCH_TARGET_MS, MAX_BATCH_SIZE);
        }
        self.sampling
            .as_ref()
            .and_then(|decision| decision.batch_size_for(&trial.workload_id, trial.engine))
            .map_or(TrialBatchConfig::SINGLE, TrialBatchConfig::fixed)
    }

    fn trial_is_active(&self, trial: &ScheduledTrial) -> bool {
        match (&self.schedule.sampling, &self.sampling) {
            (SamplingSchedule::Fixed, _) => true,
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

    pub(crate) fn next_attempt(&self, trial_id: &str) -> u32 {
        self.results
            .attempt_counts
            .get(trial_id)
            .copied()
            .unwrap_or_default()
            + 1
    }

    pub(crate) fn append_result(&self, result: &TrialResult) -> Result<()> {
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

        let mut encoded = serde_json::to_vec(result)?;
        encoded.push(b'\n');
        let path = self.root.join("trials.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        file.write_all(&encoded)
            .with_context(|| format!("failed to append {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to flush {}", path.display()))
    }

    pub(crate) fn final_results(&self, engine: Engine) -> Vec<&TrialResult> {
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

    pub(crate) fn invalid_attempts(&self, engine: Engine) -> usize {
        self.results
            .invalid_attempts
            .get(&engine)
            .copied()
            .unwrap_or_default()
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
    if let SamplingSchedule::Adaptive {
        budget_ms,
        min_final_samples,
    } = schedule.sampling
    {
        let (expected_minimum, expected_maximum) = benchmark.adaptive_final_sample_range()?;
        if budget_ms == 0
            || min_final_samples != expected_minimum
            || schedule.final_samples != expected_maximum
        {
            bail!("adaptive schedule does not match the resolved benchmark policy");
        }
    }
    let trial_ids: HashSet<_> = schedule
        .trials
        .iter()
        .map(|trial| trial.trial_id.as_str())
        .collect();
    if trial_ids.len() != schedule.trials.len() {
        bail!("schedule contains duplicate trial identifiers");
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
}

fn ingest(
    schedule: &MeasurementSchedule,
    policy: &AnalysisPolicy,
    measurement_root: &Path,
    path: &Path,
    retention_finalized: bool,
) -> Result<IngestedResults> {
    let expected: HashMap<_, _> = schedule
        .trials
        .iter()
        .map(|trial| (trial.trial_id.as_str(), trial))
        .collect();
    let source = if path.exists() {
        fs::read_to_string(path)
            .with_context(|| format!("failed to read trial results {}", path.display()))?
    } else {
        String::new()
    };
    let mut attempts: HashMap<String, Vec<TrialResult>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut environment_fingerprints = HashSet::new();

    for (line_index, line) in source.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_number = line_index + 1;
        let result: TrialResult = serde_json::from_str(line)
            .with_context(|| format!("invalid trial JSON at line {line_number}"))?;
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
        artifact_retention,
        browser_lab::ArtifactKind,
        sampling::{self, BATCH_SIZE_METRIC, CAPTURE_ELAPSED_METRIC, TRIAL_ELAPSED_METRIC},
    };

    #[test]
    fn immutable_write_is_idempotent_but_never_overwrites() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("schedule.json");
        write_immutable(&path, b"first").unwrap();
        write_immutable(&path, b"first").unwrap();
        let error = write_immutable(&path, b"second").unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
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
        assert!(!root.join("sampling.json").exists());

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
            fs::read_to_string(root.join("trials.jsonl"))
                .unwrap()
                .lines()
                .count(),
            84
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
        let preflight_marker = measurement.preflight_capture_root().join("capture.bin");
        let workload_marker = measurement.frozen_workload_root().join("checkout.json");
        fs::create_dir_all(preflight_marker.parent().unwrap()).unwrap();
        fs::create_dir_all(workload_marker.parent().unwrap()).unwrap();
        fs::write(&preflight_marker, b"capture").unwrap();
        fs::write(&workload_marker, b"workload").unwrap();

        measurement
            .commit_outcome(&serde_json::json!({"status": "open"}))
            .unwrap();
        assert!(preflight_marker.is_file());
        assert!(workload_marker.is_file());

        append_pending(&measurement);
        let completed = MeasurementSet::open(&root).unwrap();
        artifact_retention::finalize(&completed).unwrap().unwrap();
        let retained = MeasurementSet::open(&root).unwrap();
        let summary = serde_json::json!({"status": "complete"});
        retained.commit_outcome(&summary).unwrap();
        retained.commit_outcome(&summary).unwrap();

        assert!(!retained.preflight_capture_root().exists());
        assert!(!retained.frozen_workload_root().exists());
        assert!(root.join("trials.jsonl").is_file());
        assert!(root.join("artifact-retention.json").is_file());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &fs::read(root.join("summary.json")).unwrap()
            )
            .unwrap(),
            summary
        );
        MeasurementSet::open(&root).unwrap();
    }

    fn example(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
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
            let artifacts = synthetic_artifacts(measurement.root(), &trial.trial_id);
            measurement
                .append_result(&TrialResult {
                    schema_version: MEASUREMENT_SCHEMA_VERSION,
                    measurement_set_id: measurement.measurement_set_id().to_owned(),
                    trial_id: trial.trial_id.clone(),
                    attempt: 1,
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
                    artifacts,
                })
                .unwrap();
        }
    }

    fn synthetic_artifacts(root: &Path, trial_id: &str) -> Vec<ArtifactEvidence> {
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
