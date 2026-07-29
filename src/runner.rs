//! Resumable execution of one variant measurement set.

use std::{collections::BTreeMap, path::PathBuf, time::Instant};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::{BrowserLab, Engine};
use bperf_decision::environment;
use bperf_measurement::{
    MEASUREMENT_SCHEMA_VERSION,
    retention::{self as artifact_retention, RetentionSummary},
    sampling::{self, PilotStopReason, RunBudget, SamplingDecision, TRIAL_ELAPSED_METRIC},
    schedule::ScheduledTrial,
    store::{self as measurement, MeasurementSet, TrialResult},
};
use bperf_runtime::installation::BrowserInstallation;
use serde::Serialize;

use crate::benchmark_runtime::BenchmarkRuntime;

pub struct MeasureOptions {
    pub benchmark: PathBuf,
    pub variant: PathBuf,
    pub sampling: SamplingMode,
    pub artifact_root: PathBuf,
    pub runtime: BrowserInstallation,
}

pub(crate) enum SamplingMode {
    Fixed(Option<u32>),
    Adaptive {
        budget: RunBudget,
        cohort: Option<String>,
    },
}

pub(crate) fn run(options: MeasureOptions) -> Result<MeasurementOutcome> {
    let measurement_root = match options.sampling {
        SamplingMode::Fixed(final_samples) => measurement::prepare(
            &options.benchmark,
            &options.variant,
            final_samples,
            &options.artifact_root,
        )?,
        SamplingMode::Adaptive { budget, cohort } => measurement::prepare_adaptive(
            &options.benchmark,
            &options.variant,
            budget,
            cohort.as_deref(),
            &options.artifact_root,
        )?,
    };
    let mut measurement = MeasurementSet::open(&measurement_root)?;
    if lock_sampling_if_ready(&measurement)? {
        measurement = MeasurementSet::open(&measurement_root)?;
    }
    if measurement.pending_trials().is_empty() && !measurement.needs_sampling_decision() {
        return finish(&measurement);
    }

    let mut browser_lab = BrowserLab::start(options.runtime)?;
    let execution = (|| {
        let environment_fingerprint = environment::capture(&mut browser_lab, &measurement)?;
        if let Some(existing) = measurement.environment_fingerprint()
            && existing != environment_fingerprint
        {
            bail!(
                "current browser environment does not match this measurement set ({existing} != {environment_fingerprint})"
            );
        }

        let benchmark = BenchmarkRuntime::prepare(&measurement)?;
        loop {
            for trial in measurement.pending_trials() {
                eprintln!(
                    "[measure] {} attempt {}",
                    trial.trial_id,
                    measurement.next_attempt(&trial.trial_id)
                );
                let attempt = measurement.next_attempt(&trial.trial_id);
                let started = Instant::now();
                let result = benchmark.execute_trial(
                    &measurement,
                    &mut browser_lab,
                    trial,
                    attempt,
                    &environment_fingerprint,
                );
                match result {
                    Ok(mut result) => {
                        result.metrics.insert(
                            TRIAL_ELAPSED_METRIC.to_owned(),
                            (started.elapsed().as_secs_f64() * 1_000.0).max(0.001),
                        );
                        measurement.append_result(&result)?;
                        eprintln!("[measure] {} recorded", trial.trial_id);
                    }
                    Err(error) => {
                        measurement.append_result(&invalid_result(
                            &measurement,
                            trial,
                            attempt,
                            &environment_fingerprint,
                            &format!("{error:#}"),
                        ))?;
                        return Err(error).with_context(|| {
                            format!(
                                "{} attempt {} was invalid and can be resumed",
                                trial.trial_id, attempt
                            )
                        });
                    }
                }
            }

            measurement = MeasurementSet::open(&measurement_root)?;
            if lock_sampling_if_ready(&measurement)? {
                measurement = MeasurementSet::open(&measurement_root)?;
            }
            if measurement.pending_trials().is_empty() {
                break;
            }
        }
        Ok(())
    })();
    let shutdown = browser_lab.finish();
    execution?;
    shutdown?;

    let completed = MeasurementSet::open(&measurement_root)?;
    finish(&completed)
}

fn lock_sampling_if_ready(measurement: &MeasurementSet) -> Result<bool> {
    if !measurement.needs_sampling_decision() || !measurement.calibration_is_complete() {
        return Ok(false);
    }
    let decision = sampling::decide(
        measurement.schedule(),
        &measurement.benchmark().analysis_policy(),
        &measurement.calibration_results(),
    )?;
    measurement.record_sampling_decision(&decision)?;
    eprintln!(
        "[measure] adaptive sampling selected {} of {} precision-requested final trials",
        decision.selected_final_trials, decision.required_final_trials
    );
    Ok(true)
}

fn invalid_result(
    measurement: &MeasurementSet,
    trial: &ScheduledTrial,
    attempt: u32,
    environment_fingerprint: &str,
    reason: &str,
) -> TrialResult {
    TrialResult {
        schema_version: MEASUREMENT_SCHEMA_VERSION,
        measurement_set_id: measurement.measurement_set_id().to_owned(),
        trial_id: trial.trial_id.clone(),
        attempt,
        workload_id: trial.workload_id.clone(),
        engine: trial.engine,
        phase: trial.phase,
        sample_index: trial.sample_index,
        environment_fingerprint: environment_fingerprint.to_owned(),
        valid: false,
        success: false,
        failure_category: None,
        failure_detail: None,
        invalidation_reason: Some(reason.to_owned()),
        metrics: BTreeMap::new(),
        artifacts: Vec::new(),
    }
}

#[derive(Serialize)]
pub(crate) struct MeasurementOutcome {
    schema_version: u32,
    status: &'static str,
    measurement_set_id: String,
    measurement_root: PathBuf,
    benchmark_id: String,
    variant_id: String,
    completed_trials: usize,
    total_trials: usize,
    final_complete: bool,
    environment_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sampling: Option<SamplingDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_retention: Option<RetentionSummary>,
}

impl MeasurementOutcome {
    pub(crate) fn report(&self, command: &str, json: bool) -> Result<()> {
        let encoded = serde_json::to_string_pretty(self)?;
        if json {
            println!("{encoded}");
        } else {
            println!("bperf {command}: {}", self.status);
            self.report_details();
        }
        Ok(())
    }

    pub(crate) fn report_details(&self) {
        println!(
            "  {}/{} trials recorded",
            self.completed_trials, self.total_trials
        );
        if let Some(sampling) = &self.sampling {
            let pilot_trials = sampling
                .strata
                .iter()
                .map(|stratum| stratum.pilot_samples)
                .sum::<u32>();
            let stable_strata = sampling
                .strata
                .iter()
                .filter(|stratum| stratum.pilot_stop_reason == PilotStopReason::Stable)
                .count();
            println!(
                "  adaptive calibration: {pilot_trials} pilot trials; \
                 {stable_strata}/{} strata met the stability rule",
                sampling.strata.len()
            );
            let qualifier = if sampling.budget_limited {
                " (budget-limited)"
            } else {
                ""
            };
            println!(
                "  adaptive sampling: {} final trials{}",
                sampling.selected_final_trials, qualifier
            );
        }
        if let Some(retention) = &self.artifact_retention {
            println!(
                "  artifacts: {} representative retained, {} discarded",
                retention.retained_artifacts, retention.discarded_artifacts
            );
            println!(
                "  profiles: {}",
                self.measurement_root
                    .join("artifact-retention.json")
                    .display()
            );
        }
        if self.sampling.is_some() {
            println!(
                "  sampling: {}",
                self.measurement_root.join("sampling.json").display()
            );
        }
        println!(
            "  measurement: {}",
            self.measurement_root.join("summary.json").display()
        );
    }

    pub(crate) fn report_engine_results(&self) -> Result<()> {
        let measurement = MeasurementSet::open(&self.measurement_root)?;
        for engine in Engine::ALL {
            let expected = measurement.expected_final_trials(engine);
            let results = measurement.final_results(engine);
            let successful = results.iter().filter(|result| result.success).count();
            let correctness = if expected == 0 || results.len() < expected {
                "inconclusive"
            } else if successful == expected {
                "pass"
            } else {
                "fail"
            };
            println!(
                "  {engine}: measured correctness={correctness} final={}/{} invalid_attempts={}",
                results.len(),
                expected,
                measurement.invalid_attempts(engine)
            );
        }
        Ok(())
    }

    pub(crate) fn measurement_root(&self) -> &std::path::Path {
        &self.measurement_root
    }

    pub(crate) fn measurement_set_id(&self) -> &str {
        &self.measurement_set_id
    }

    pub(crate) fn benchmark_id(&self) -> &str {
        &self.benchmark_id
    }
}

fn finish(measurement: &MeasurementSet) -> Result<MeasurementOutcome> {
    let pending = measurement.pending_trials().len();
    let total = measurement.active_trial_count();
    let complete = pending == 0 && !measurement.needs_sampling_decision();
    let artifact_retention = artifact_retention::finalize(measurement)?;
    let summary = MeasurementOutcome {
        schema_version: MEASUREMENT_SCHEMA_VERSION,
        status: if complete { "complete" } else { "open" },
        measurement_set_id: measurement.measurement_set_id().to_owned(),
        measurement_root: measurement.root().to_owned(),
        benchmark_id: measurement.benchmark_id().to_owned(),
        variant_id: measurement.variant_id().to_owned(),
        completed_trials: measurement.completed_active_trial_count(),
        total_trials: total,
        final_complete: measurement.final_is_complete(),
        environment_fingerprint: measurement.environment_fingerprint().map(str::to_owned),
        sampling: measurement.sampling_decision().cloned(),
        artifact_retention,
    };
    MeasurementSet::open(measurement.root())?.commit_outcome(&summary)?;
    Ok(summary)
}
