//! Comparative analysis over two immutable measurement sets.

use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::Engine;
use bperf_measurement::{
    manifest::{AnalysisPolicy, MetricPolicy},
    store::{MeasurementSet, TrialResult},
};
use bperf_storage::database::Database;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    baseline,
    environment::{self, EnvironmentPair},
};

const ANCHOR_MAX_DRIFT_PCT: f64 = 5.0;
const ANCHOR_BOOTSTRAP_SAMPLES: u32 = 5_000;
const BASELINE_AGE_WARNING_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const COMPARISON_DOCUMENTS: &str = "comparison";

pub struct CompareOptions {
    pub candidate_root: PathBuf,
    pub baseline_root: Option<PathBuf>,
    pub registry_root: PathBuf,
    pub artifact_root: PathBuf,
    pub output: Option<PathBuf>,
}

pub fn run(options: CompareOptions) -> Result<ComparisonOutcome> {
    let candidate = MeasurementSet::open(&options.candidate_root)?;
    let baseline_root = match options.baseline_root {
        Some(path) => path,
        None => baseline::current_path(&options.registry_root, candidate.benchmark_id())?,
    };
    let baseline = MeasurementSet::open(&baseline_root)?;
    validate_compatibility(&baseline, &candidate)?;
    let environments = environment::compatible_pair(&baseline, &candidate)?;

    let comparison_id = comparison_id(&baseline, &candidate);
    let report = compare(&comparison_id, &baseline, &candidate, environments.as_ref())?;
    let database = Database::for_collection(&options.artifact_root, "comparisons")?;
    database.publish_document(COMPARISON_DOCUMENTS, &comparison_id, &report)?;
    let output_path = options
        .output
        .map(|path| {
            let encoded = format!("{}\n", serde_json::to_string_pretty(&report)?);
            bperf_storage::replace_file(&path, encoded.as_bytes())
                .with_context(|| format!("failed to write comparison export {}", path.display()))?;
            Ok::<_, anyhow::Error>(path)
        })
        .transpose()?;

    Ok(ComparisonOutcome {
        report,
        output_path,
    })
}

pub struct ComparisonOutcome {
    report: ComparisonReport,
    output_path: Option<PathBuf>,
}

impl ComparisonOutcome {
    pub fn report(&self, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(&self.report)?);
        } else {
            println!("bperf compare: {}", self.report.verdict.as_str());
            self.report_details();
        }
        Ok(())
    }

    pub fn report_details(&self) {
        println!(
            "  baseline: {} ({})",
            self.report.baseline.variant_id, self.report.baseline.measurement_set_id
        );
        println!(
            "  candidate: {} ({})",
            self.report.candidate.variant_id, self.report.candidate.measurement_set_id
        );
        print!("{}", self.summary().render_decision_summary());
        println!("  comparison: {}", self.report.comparison_id);
        if let Some(path) = &self.output_path {
            println!("  comparison export: {}", path.display());
        }
    }

    pub fn exit_code(&self) -> std::process::ExitCode {
        std::process::ExitCode::from(self.exit_code_value())
    }

    pub fn exit_code_value(&self) -> u8 {
        match self.report.verdict {
            Verdict::Positive | Verdict::Equivalent => 0,
            Verdict::Negative => 1,
            Verdict::Inconclusive => 2,
        }
    }

    pub fn comparison_id(&self) -> &str {
        &self.report.comparison_id
    }

    pub fn report_data(&self) -> &ComparisonReport {
        &self.report
    }

    pub fn summary(&self) -> ComparisonSummary {
        ComparisonSummary {
            comparison_id: self.report.comparison_id.clone(),
            report_path: self
                .output_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            baseline_measurement_set: self.report.baseline.measurement_set_id.clone(),
            candidate_measurement_set: self.report.candidate.measurement_set_id.clone(),
            environment_fingerprint: self.report.environment_fingerprint.clone(),
            policy: "strict_all".to_owned(),
            verdict: self.report.verdict.as_str().to_owned(),
            engines: self
                .report
                .engines
                .iter()
                .map(|engine| EngineSummary {
                    engine: engine.engine,
                    verdict: engine.verdict.as_str().to_owned(),
                    correctness: engine.correctness.gate.as_str().to_owned(),
                    anchor: Some(AnchorSummary {
                        status: engine.anchor.status.as_str().to_owned(),
                        drift_pct: engine.anchor.drift_pct,
                        ci_pct: engine.anchor.ci_pct,
                    }),
                    metrics: engine
                        .effects
                        .iter()
                        .map(|(name, effect)| {
                            (
                                name.clone(),
                                MetricSummary {
                                    improvement_pct: effect.improvement_pct,
                                    ci_pct: effect.ci_pct,
                                    classification: effect.classification.as_str().to_owned(),
                                    guardrail_regressed: effect.guardrail_regressed,
                                    baseline_value: effect.baseline_value,
                                    candidate_value: effect.candidate_value,
                                },
                            )
                        })
                        .collect(),
                })
                .collect(),
            warnings: self.report.warnings.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonSummary {
    pub comparison_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_path: Option<String>,
    pub baseline_measurement_set: String,
    pub candidate_measurement_set: String,
    pub environment_fingerprint: Option<String>,
    pub policy: String,
    pub verdict: String,
    pub engines: Vec<EngineSummary>,
    pub warnings: Vec<String>,
}

impl ComparisonSummary {
    pub(crate) fn render_decision_summary(&self) -> String {
        let mut output = String::new();
        for engine in &self.engines {
            let anchor = engine.anchor.as_ref().map_or_else(
                || "unreported".to_owned(),
                |anchor| {
                    anchor.drift_pct.map_or_else(
                        || anchor.status.clone(),
                        |drift| format!("{} ({drift:+.2}%)", anchor.status),
                    )
                },
            );
            let _ = writeln!(
                output,
                "  {}: {} correctness={} anchor={anchor}",
                engine.engine, engine.verdict, engine.correctness,
            );
            for (metric, effect) in &engine.metrics {
                let improvement = effect
                    .improvement_pct
                    .map_or_else(|| "n/a".to_owned(), |value| format!("{value:+.2}%"));
                let interval = effect.ci_pct.map_or_else(
                    || "n/a".to_owned(),
                    |[low, high]| format!("[{low:+.2}%, {high:+.2}%]"),
                );
                let guardrail = if effect.guardrail_regressed {
                    " guardrail=regressed"
                } else {
                    ""
                };
                let values = effect
                    .baseline_value
                    .zip(effect.candidate_value)
                    .map_or_else(String::new, |(baseline, candidate)| {
                        format!(" ({})", format_metric_values(metric, baseline, candidate))
                    });
                let _ = writeln!(
                    output,
                    "    {metric}: {} effect={improvement}{values} ci={interval}{guardrail}",
                    effect.classification
                );
            }
        }
        for warning in &self.warnings {
            let _ = writeln!(output, "  warning: {warning}");
        }
        output
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineSummary {
    pub engine: Engine,
    pub verdict: String,
    pub correctness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnchorSummary>,
    pub metrics: BTreeMap<String, MetricSummary>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnchorSummary {
    pub status: String,
    pub drift_pct: Option<f64>,
    pub ci_pct: Option<[f64; 2]>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSummary {
    pub improvement_pct: Option<f64>,
    pub ci_pct: Option<[f64; 2]>,
    pub classification: String,
    pub guardrail_regressed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_value: Option<f64>,
}

fn format_metric_values(metric: &str, baseline: f64, candidate: f64) -> String {
    let magnitude = baseline.abs().max(candidate.abs());
    let (scale, unit) = if metric.ends_with("_ms") {
        if magnitude < 0.001 {
            (1_000_000.0, "ns")
        } else if magnitude < 1.0 {
            (1_000.0, "us")
        } else if magnitude < 1_000.0 {
            (1.0, "ms")
        } else {
            (0.001, "s")
        }
    } else if metric.ends_with("_bytes") {
        if magnitude < 1_024.0 {
            (1.0, "b")
        } else {
            (1.0 / 1_024.0, "kb")
        }
    } else {
        (1.0, "")
    };
    format!(
        "{}{unit} -> {}{unit}",
        format_metric_number(baseline * scale),
        format_metric_number(candidate * scale)
    )
}

fn format_metric_number(value: f64) -> String {
    let number = format!("{value:.3}");
    number
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn validate_compatibility(baseline: &MeasurementSet, candidate: &MeasurementSet) -> Result<()> {
    if baseline.benchmark_id() != candidate.benchmark_id()
        || baseline.subject_id() != candidate.subject_id()
    {
        bail!("measurement sets describe different benchmark subjects");
    }
    if baseline.benchmark_sha256() != candidate.benchmark_sha256() {
        bail!(
            "measurement sets use different benchmark specifications; workloads, engines, capture policy, and statistical policy must match"
        );
    }
    Ok(())
}

fn comparison_id(baseline: &MeasurementSet, candidate: &MeasurementSet) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bperf-comparison-v1\0");
    digest.update(baseline.measurement_set_id().as_bytes());
    digest.update([0]);
    digest.update(candidate.measurement_set_id().as_bytes());
    digest.update([0]);
    digest.update(baseline.benchmark_sha256().as_bytes());
    format!("compare-{:x}", digest.finalize())[..28].to_owned()
}

fn compare(
    comparison_id: &str,
    baseline: &MeasurementSet,
    candidate: &MeasurementSet,
    environments: Option<&EnvironmentPair>,
) -> Result<ComparisonReport> {
    let policy = candidate.benchmark().analysis_policy();
    let workloads: Vec<_> = candidate.benchmark().workload_ids().collect();
    let seed = comparison_seed(baseline, candidate);
    let stability = stability_analysis(
        candidate.benchmark().engines(),
        environments,
        policy.confidence,
        seed,
    )?;
    let engines: Vec<_> = candidate
        .benchmark()
        .engines()
        .iter()
        .zip(&stability.engines)
        .map(|(engine, anchor)| {
            analyze_engine(
                *engine,
                &workloads,
                EngineInput::new(baseline, *engine),
                EngineInput::new(candidate, *engine),
                &policy,
                seed,
                anchor.clone(),
            )
        })
        .collect();
    let verdict = rollup(&engines);
    let mut warnings = Vec::new();
    for engine in &engines {
        if engine.baseline.completed_trials < engine.baseline.expected_trials {
            warnings.push(format!(
                "{} baseline is incomplete: {}/{} final trials",
                engine.engine, engine.baseline.completed_trials, engine.baseline.expected_trials
            ));
        }
        if engine.candidate.completed_trials < engine.candidate.expected_trials {
            warnings.push(format!(
                "{} candidate is incomplete: {}/{} final trials",
                engine.engine, engine.candidate.completed_trials, engine.candidate.expected_trials
            ));
        }
    }
    if baseline.environment_fingerprint().is_none() || candidate.environment_fingerprint().is_none()
    {
        warnings.push(
            "environment compatibility is unproven until both measurement sets contain results"
                .to_owned(),
        );
    }
    for anchor in &stability.engines {
        if anchor.status != AnchorStatus::Stable {
            warnings.push(format!(
                "{} runtime anchor is {}; historical performance comparison is inconclusive",
                anchor.engine,
                anchor.status.as_str()
            ));
        }
    }
    if stability.baseline_age_ms > BASELINE_AGE_WARNING_MS {
        warnings.push(format!(
            "baseline evidence is {:.1} days old",
            stability.baseline_age_ms as f64 / (24.0 * 60.0 * 60.0 * 1_000.0)
        ));
    }

    Ok(ComparisonReport {
        schema_version: 2,
        comparison_id: comparison_id.to_owned(),
        method: "independent_two_sample_hierarchical_bootstrap",
        benchmark_id: candidate.benchmark_id().to_owned(),
        subject_id: candidate.subject_id().to_owned(),
        benchmark_sha256: candidate.benchmark_sha256().to_owned(),
        baseline: MeasurementReference::new(baseline),
        candidate: MeasurementReference::new(candidate),
        environment_fingerprint: candidate
            .environment_fingerprint()
            .or_else(|| baseline.environment_fingerprint())
            .map(str::to_owned),
        confidence: policy.confidence,
        bootstrap_samples: policy.bootstrap_samples,
        stability,
        verdict,
        engines,
        warnings,
    })
}

fn comparison_seed(baseline: &MeasurementSet, candidate: &MeasurementSet) -> u64 {
    let mut digest = Sha256::new();
    digest.update(b"bperf-comparison-seed-v1\0");
    digest.update(baseline.measurement_set_id().as_bytes());
    digest.update([0]);
    digest.update(candidate.measurement_set_id().as_bytes());
    u64::from_le_bytes(digest.finalize()[..8].try_into().unwrap())
}

struct EngineInput<'a> {
    expected: usize,
    results: Vec<&'a TrialResult>,
    invalid_attempts: usize,
}

impl<'a> EngineInput<'a> {
    fn new(measurement: &'a MeasurementSet, engine: Engine) -> Self {
        Self {
            expected: measurement.expected_final_trials(engine),
            results: measurement.final_results(engine),
            invalid_attempts: measurement.invalid_attempts(engine),
        }
    }

    fn complete(&self) -> bool {
        self.results.len() == self.expected
    }
}

fn stability_analysis(
    engines: &[Engine],
    environments: Option<&EnvironmentPair>,
    confidence: f64,
    seed: u64,
) -> Result<StabilityAnalysis> {
    let Some(environments) = environments else {
        return Ok(StabilityAnalysis {
            status: AnchorStatus::Unproven,
            max_drift_pct: ANCHOR_MAX_DRIFT_PCT,
            baseline_recorded_at_unix_ms: None,
            candidate_recorded_at_unix_ms: None,
            baseline_age_ms: 0,
            engines: engines
                .iter()
                .map(|engine| AnchorAnalysis::unproven(*engine))
                .collect(),
        });
    };
    let baseline_recorded_at_unix_ms = environments.baseline.recorded_at_unix_ms();
    let candidate_recorded_at_unix_ms = environments.candidate.recorded_at_unix_ms();
    let analyses: Vec<_> = engines
        .iter()
        .map(|engine| {
            Ok(analyze_anchor(
                *engine,
                environments.baseline.anchor(*engine)?,
                environments.candidate.anchor(*engine)?,
                confidence,
                derived_seed(seed, &format!("{engine}:runtime-anchor")),
            ))
        })
        .collect::<Result<_>>()?;
    let status = if analyses
        .iter()
        .all(|analysis| analysis.status == AnchorStatus::Stable)
    {
        AnchorStatus::Stable
    } else if analyses
        .iter()
        .any(|analysis| analysis.status == AnchorStatus::Drifted)
    {
        AnchorStatus::Drifted
    } else {
        AnchorStatus::Inconclusive
    };
    Ok(StabilityAnalysis {
        status,
        max_drift_pct: ANCHOR_MAX_DRIFT_PCT,
        baseline_recorded_at_unix_ms: Some(baseline_recorded_at_unix_ms),
        candidate_recorded_at_unix_ms: Some(candidate_recorded_at_unix_ms),
        baseline_age_ms: candidate_recorded_at_unix_ms.saturating_sub(baseline_recorded_at_unix_ms),
        engines: analyses,
    })
}

fn analyze_anchor(
    engine: Engine,
    baseline: &[f64],
    candidate: &[f64],
    confidence: f64,
    seed: u64,
) -> AnchorAnalysis {
    let baseline_median_ms = median(baseline);
    let candidate_median_ms = median(candidate);
    let drift_pct = duration_change_pct(baseline_median_ms, candidate_median_ms);
    let mut rng = SplitMix64::new(seed);
    let mut estimates = Vec::with_capacity(ANCHOR_BOOTSTRAP_SAMPLES as usize);
    for _ in 0..ANCHOR_BOOTSTRAP_SAMPLES {
        estimates.push(duration_change_pct(
            resampled_median(baseline, &mut rng),
            resampled_median(candidate, &mut rng),
        ));
    }
    estimates.sort_by(f64::total_cmp);
    let alpha = (1.0 - confidence) / 2.0;
    let ci_pct = [
        quantile_sorted(&estimates, alpha),
        quantile_sorted(&estimates, 1.0 - alpha),
    ];
    let status = if ci_pct[0] >= -ANCHOR_MAX_DRIFT_PCT && ci_pct[1] <= ANCHOR_MAX_DRIFT_PCT {
        AnchorStatus::Stable
    } else if ci_pct[0] > ANCHOR_MAX_DRIFT_PCT || ci_pct[1] < -ANCHOR_MAX_DRIFT_PCT {
        AnchorStatus::Drifted
    } else {
        AnchorStatus::Inconclusive
    };
    AnchorAnalysis {
        engine,
        status,
        baseline_samples: baseline.len(),
        candidate_samples: candidate.len(),
        baseline_median_ms: Some(baseline_median_ms),
        candidate_median_ms: Some(candidate_median_ms),
        drift_pct: Some(drift_pct),
        ci_pct: Some(ci_pct),
    }
}

fn analyze_engine(
    engine: Engine,
    workloads: &[&str],
    baseline: EngineInput<'_>,
    candidate: EngineInput<'_>,
    policy: &AnalysisPolicy,
    seed: u64,
    anchor: AnchorAnalysis,
) -> EngineAnalysis {
    let complete = baseline.complete() && candidate.complete();
    let correctness = correctness(
        workloads,
        &baseline.results,
        &candidate.results,
        complete,
        policy,
        derived_seed(seed, &format!("{engine}:correctness")),
    );
    let effects: BTreeMap<_, _> = policy
        .primary_metrics
        .iter()
        .map(|metric| {
            (
                metric.name.clone(),
                metric_effect(
                    workloads,
                    &baseline.results,
                    &candidate.results,
                    metric,
                    policy,
                    derived_seed(seed, &format!("{engine}:{}", metric.name)),
                ),
            )
        })
        .collect();
    let metric_verdict = engine_verdict(complete, correctness.gate, effects.values());
    let verdict = if correctness.gate == Gate::Fail {
        Verdict::Negative
    } else if anchor.status != AnchorStatus::Stable {
        Verdict::Inconclusive
    } else {
        metric_verdict
    };
    EngineAnalysis {
        engine,
        verdict,
        anchor,
        baseline: MeasurementCompletion {
            expected_trials: baseline.expected,
            completed_trials: baseline.results.len(),
            invalid_attempts: baseline.invalid_attempts,
        },
        candidate: MeasurementCompletion {
            expected_trials: candidate.expected,
            completed_trials: candidate.results.len(),
            invalid_attempts: candidate.invalid_attempts,
        },
        correctness,
        effects,
    }
}

fn correctness(
    workloads: &[&str],
    baseline_results: &[&TrialResult],
    candidate_results: &[&TrialResult],
    complete: bool,
    policy: &AnalysisPolicy,
    seed: u64,
) -> CorrectnessAnalysis {
    let baseline_successes = baseline_results
        .iter()
        .filter(|trial| trial.success)
        .count();
    let candidate_successes = candidate_results
        .iter()
        .filter(|trial| trial.success)
        .count();
    let baseline = SuccessRate::new(
        baseline_successes,
        baseline_results.len(),
        policy.confidence,
    );
    let candidate = SuccessRate::new(
        candidate_successes,
        candidate_results.len(),
        policy.confidence,
    );
    let baseline_groups = grouped_values(baseline_results, |trial| u8::from(trial.success) as f64);
    let candidate_groups =
        grouped_values(candidate_results, |trial| u8::from(trial.success) as f64);
    let comparable =
        groups_cover(workloads, &baseline_groups) && groups_cover(workloads, &candidate_groups);
    let (delta_percentage_points, delta_ci_percentage_points) = if comparable {
        let point =
            workload_weighted_difference(workloads, &candidate_groups, &baseline_groups) * 100.0;
        let interval = independent_bootstrap_interval(
            workloads,
            &candidate_groups,
            &baseline_groups,
            policy.confidence,
            policy.bootstrap_samples,
            seed,
        );
        (
            Some(point),
            Some([interval[0] * 100.0, interval[1] * 100.0]),
        )
    } else {
        (None, None)
    };

    let gate = if !complete || !comparable {
        Gate::Inconclusive
    } else {
        let candidate_rate = candidate.percentage.unwrap_or_default() / 100.0;
        let interval = delta_ci_percentage_points.unwrap();
        let margin = policy.max_regression_percentage_points;
        if candidate_rate < policy.minimum_success_rate || interval[1] < -margin {
            Gate::Fail
        } else if interval[0] >= -margin {
            Gate::Pass
        } else {
            Gate::Inconclusive
        }
    };
    CorrectnessAnalysis {
        gate,
        baseline,
        candidate,
        delta_percentage_points,
        delta_ci_percentage_points,
        max_regression_percentage_points: policy.max_regression_percentage_points,
        minimum_success_rate: policy.minimum_success_rate,
    }
}

fn metric_effect(
    workloads: &[&str],
    baseline_results: &[&TrialResult],
    candidate_results: &[&TrialResult],
    metric: &MetricPolicy,
    policy: &AnalysisPolicy,
    seed: u64,
) -> MetricEffect {
    let baseline_values = grouped_successful_metrics(baseline_results, &metric.name);
    let candidate_values = grouped_successful_metrics(candidate_results, &metric.name);
    if !groups_cover(workloads, &baseline_values) || !groups_cover(workloads, &candidate_values) {
        return MetricEffect::insufficient(metric.minimum_effect_pct);
    }

    let baseline_logs = map_groups(&baseline_values, f64::ln);
    let candidate_logs = map_groups(&candidate_values, f64::ln);
    let baseline_log_value = workload_weighted_mean(workloads, &baseline_logs);
    let candidate_log_value = workload_weighted_mean(workloads, &candidate_logs);
    let point_log = baseline_log_value - candidate_log_value;
    let log_interval = independent_bootstrap_interval(
        workloads,
        &baseline_logs,
        &candidate_logs,
        policy.confidence,
        policy.bootstrap_samples,
        seed,
    );
    let improvement_pct = log_to_improvement(point_log);
    let ci_pct = [
        log_to_improvement(log_interval[0]),
        log_to_improvement(log_interval[1]),
    ];
    let baseline_flat: Vec<_> = baseline_values.values().flatten().copied().collect();
    let candidate_flat: Vec<_> = candidate_values.values().flatten().copied().collect();
    let classification = classify(improvement_pct, ci_pct, metric.minimum_effect_pct);
    MetricEffect {
        baseline_samples: baseline_flat.len(),
        candidate_samples: candidate_flat.len(),
        baseline: Some(DistributionSummary::new(&baseline_flat)),
        candidate: Some(DistributionSummary::new(&candidate_flat)),
        improvement_pct: Some(improvement_pct),
        ci_pct: Some(ci_pct),
        minimum_effect_pct: metric.minimum_effect_pct,
        guardrail_regressed: ci_pct[1] < -policy.protected_metric_max_regression_pct,
        classification,
        baseline_value: Some(baseline_log_value.exp()),
        candidate_value: Some(candidate_log_value.exp()),
    }
}

fn grouped_values(
    results: &[&TrialResult],
    value: impl Fn(&TrialResult) -> f64,
) -> BTreeMap<String, Vec<f64>> {
    let mut groups: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for trial in results {
        groups
            .entry(trial.workload_id.clone())
            .or_default()
            .push(value(trial));
    }
    groups
}

fn grouped_successful_metrics(
    results: &[&TrialResult],
    metric: &str,
) -> BTreeMap<String, Vec<f64>> {
    let mut groups: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for trial in results.iter().filter(|trial| trial.success) {
        groups
            .entry(trial.workload_id.clone())
            .or_default()
            .push(trial.metrics[metric]);
    }
    groups
}

fn map_groups(
    groups: &BTreeMap<String, Vec<f64>>,
    transform: impl Fn(f64) -> f64,
) -> BTreeMap<String, Vec<f64>> {
    groups
        .iter()
        .map(|(key, values)| {
            (
                key.clone(),
                values.iter().copied().map(&transform).collect(),
            )
        })
        .collect()
}

fn groups_cover(workloads: &[&str], groups: &BTreeMap<String, Vec<f64>>) -> bool {
    workloads.iter().all(|workload| {
        groups
            .get(*workload)
            .is_some_and(|values| !values.is_empty())
    })
}

fn workload_weighted_difference(
    workloads: &[&str],
    left: &BTreeMap<String, Vec<f64>>,
    right: &BTreeMap<String, Vec<f64>>,
) -> f64 {
    workloads
        .iter()
        .map(|workload| mean(&left[*workload]) - mean(&right[*workload]))
        .sum::<f64>()
        / workloads.len() as f64
}

fn workload_weighted_mean(workloads: &[&str], groups: &BTreeMap<String, Vec<f64>>) -> f64 {
    workloads
        .iter()
        .map(|workload| mean(&groups[*workload]))
        .sum::<f64>()
        / workloads.len() as f64
}

fn independent_bootstrap_interval(
    workloads: &[&str],
    left: &BTreeMap<String, Vec<f64>>,
    right: &BTreeMap<String, Vec<f64>>,
    confidence: f64,
    samples: u32,
    seed: u64,
) -> [f64; 2] {
    let mut rng = SplitMix64::new(seed);
    let mut estimates = Vec::with_capacity(samples as usize);
    for _ in 0..samples {
        let mut workload_total = 0.0;
        for _ in 0..workloads.len() {
            let workload = workloads[rng.index(workloads.len())];
            workload_total += resampled_mean(&left[workload], &mut rng)
                - resampled_mean(&right[workload], &mut rng);
        }
        estimates.push(workload_total / workloads.len() as f64);
    }
    estimates.sort_by(f64::total_cmp);
    let alpha = (1.0 - confidence) / 2.0;
    [
        quantile_sorted(&estimates, alpha),
        quantile_sorted(&estimates, 1.0 - alpha),
    ]
}

fn resampled_mean(values: &[f64], rng: &mut SplitMix64) -> f64 {
    (0..values.len())
        .map(|_| values[rng.index(values.len())])
        .sum::<f64>()
        / values.len() as f64
}

fn classify(point: f64, interval: [f64; 2], minimum_effect: f64) -> Classification {
    if interval[0] > 0.0 && point >= minimum_effect {
        Classification::Improved
    } else if interval[1] < 0.0 && point <= -minimum_effect {
        Classification::Regressed
    } else if interval[0] >= -minimum_effect && interval[1] <= minimum_effect {
        Classification::Equivalent
    } else {
        Classification::Inconclusive
    }
}

fn engine_verdict<'a>(
    complete: bool,
    gate: Gate,
    effects: impl Iterator<Item = &'a MetricEffect>,
) -> Verdict {
    if !complete || gate == Gate::Inconclusive {
        return Verdict::Inconclusive;
    }
    if gate == Gate::Fail {
        return Verdict::Negative;
    }
    let effects: Vec<_> = effects.collect();
    if effects.iter().any(|effect| {
        effect.classification == Classification::Regressed || effect.guardrail_regressed
    }) {
        Verdict::Negative
    } else if effects
        .iter()
        .all(|effect| effect.classification == Classification::Equivalent)
    {
        Verdict::Equivalent
    } else if effects
        .iter()
        .any(|effect| effect.classification == Classification::Improved)
        && effects.iter().all(|effect| {
            matches!(
                effect.classification,
                Classification::Improved | Classification::Equivalent
            )
        })
    {
        Verdict::Positive
    } else {
        Verdict::Inconclusive
    }
}

fn rollup(engines: &[EngineAnalysis]) -> Verdict {
    if engines
        .iter()
        .any(|engine| engine.verdict == Verdict::Negative)
    {
        Verdict::Negative
    } else if engines
        .iter()
        .all(|engine| engine.verdict == Verdict::Positive)
    {
        Verdict::Positive
    } else if engines
        .iter()
        .all(|engine| engine.verdict == Verdict::Equivalent)
    {
        Verdict::Equivalent
    } else {
        Verdict::Inconclusive
    }
}

fn derived_seed(seed: u64, scope: &str) -> u64 {
    let mut digest = Sha256::new();
    digest.update(b"bperf-independent-bootstrap-v1\0");
    digest.update(seed.to_le_bytes());
    digest.update([0]);
    digest.update(scope.as_bytes());
    u64::from_le_bytes(digest.finalize()[..8].try_into().unwrap())
}

fn log_to_improvement(log_ratio: f64) -> f64 {
    100.0 * (1.0 - (-log_ratio).exp())
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn median(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    quantile_sorted(&values, 0.5)
}

fn resampled_median(values: &[f64], rng: &mut SplitMix64) -> f64 {
    let mut sample: Vec<_> = (0..values.len())
        .map(|_| values[rng.index(values.len())])
        .collect();
    sample.sort_by(f64::total_cmp);
    quantile_sorted(&sample, 0.5)
}

fn duration_change_pct(baseline: f64, candidate: f64) -> f64 {
    100.0 * (candidate / baseline - 1.0)
}

fn quantile_sorted(values: &[f64], probability: f64) -> f64 {
    let position = probability.clamp(0.0, 1.0) * (values.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        values[lower]
    } else {
        values[lower] + (values[upper] - values[lower]) * (position - lower as f64)
    }
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, upper: usize) -> usize {
        assert!(upper > 0);
        let bound = upper as u64;
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let value = self.next();
            if value >= threshold {
                return (value % bound) as usize;
            }
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ComparisonReport {
    schema_version: u32,
    comparison_id: String,
    method: &'static str,
    benchmark_id: String,
    subject_id: String,
    benchmark_sha256: String,
    baseline: MeasurementReference,
    candidate: MeasurementReference,
    environment_fingerprint: Option<String>,
    confidence: f64,
    bootstrap_samples: u32,
    stability: StabilityAnalysis,
    verdict: Verdict,
    engines: Vec<EngineAnalysis>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct MeasurementReference {
    measurement_set_id: String,
    variant_id: String,
    variant_sha256: String,
    path: String,
}

impl MeasurementReference {
    fn new(measurement: &MeasurementSet) -> Self {
        Self {
            measurement_set_id: measurement.measurement_set_id().to_owned(),
            variant_id: measurement.variant_id().to_owned(),
            variant_sha256: measurement.variant_sha256().to_owned(),
            path: measurement.root().to_string_lossy().into_owned(),
        }
    }
}

#[derive(Debug, Serialize)]
struct EngineAnalysis {
    engine: Engine,
    verdict: Verdict,
    anchor: AnchorAnalysis,
    baseline: MeasurementCompletion,
    candidate: MeasurementCompletion,
    correctness: CorrectnessAnalysis,
    effects: BTreeMap<String, MetricEffect>,
}

#[derive(Clone, Debug, Serialize)]
struct StabilityAnalysis {
    status: AnchorStatus,
    max_drift_pct: f64,
    baseline_recorded_at_unix_ms: Option<u64>,
    candidate_recorded_at_unix_ms: Option<u64>,
    baseline_age_ms: u64,
    engines: Vec<AnchorAnalysis>,
}

#[derive(Clone, Debug, Serialize)]
struct AnchorAnalysis {
    engine: Engine,
    status: AnchorStatus,
    baseline_samples: usize,
    candidate_samples: usize,
    baseline_median_ms: Option<f64>,
    candidate_median_ms: Option<f64>,
    drift_pct: Option<f64>,
    ci_pct: Option<[f64; 2]>,
}

impl AnchorAnalysis {
    fn unproven(engine: Engine) -> Self {
        Self {
            engine,
            status: AnchorStatus::Unproven,
            baseline_samples: 0,
            candidate_samples: 0,
            baseline_median_ms: None,
            candidate_median_ms: None,
            drift_pct: None,
            ci_pct: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AnchorStatus {
    Stable,
    Drifted,
    Inconclusive,
    Unproven,
}

impl AnchorStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Drifted => "drifted",
            Self::Inconclusive => "inconclusive",
            Self::Unproven => "unproven",
        }
    }
}

#[derive(Debug, Serialize)]
struct MeasurementCompletion {
    expected_trials: usize,
    completed_trials: usize,
    invalid_attempts: usize,
}

#[derive(Debug, Serialize)]
struct CorrectnessAnalysis {
    gate: Gate,
    baseline: SuccessRate,
    candidate: SuccessRate,
    delta_percentage_points: Option<f64>,
    delta_ci_percentage_points: Option<[f64; 2]>,
    max_regression_percentage_points: f64,
    minimum_success_rate: f64,
}

#[derive(Debug, Serialize)]
struct SuccessRate {
    successful: usize,
    attempted: usize,
    percentage: Option<f64>,
    wilson_ci_pct: Option<[f64; 2]>,
}

impl SuccessRate {
    fn new(successful: usize, attempted: usize, confidence: f64) -> Self {
        if attempted == 0 {
            return Self {
                successful,
                attempted,
                percentage: None,
                wilson_ci_pct: None,
            };
        }
        let proportion = successful as f64 / attempted as f64;
        let z = inverse_normal(0.5 + confidence / 2.0);
        let denominator = 1.0 + z * z / attempted as f64;
        let center = (proportion + z * z / (2.0 * attempted as f64)) / denominator;
        let spread = z
            * ((proportion * (1.0 - proportion) / attempted as f64
                + z * z / (4.0 * (attempted as f64).powi(2)))
            .sqrt())
            / denominator;
        Self {
            successful,
            attempted,
            percentage: Some(proportion * 100.0),
            wilson_ci_pct: Some([(center - spread) * 100.0, (center + spread) * 100.0]),
        }
    }
}

fn inverse_normal(probability: f64) -> f64 {
    debug_assert!((0.0..1.0).contains(&probability));
    let (tail, sign) = if probability < 0.5 {
        (probability, -1.0)
    } else {
        (1.0 - probability, 1.0)
    };
    let t = (-2.0 * tail.ln()).sqrt();
    let numerator = 2.515_517 + 0.802_853 * t + 0.010_328 * t * t;
    let denominator = 1.0 + 1.432_788 * t + 0.189_269 * t * t + 0.001_308 * t * t * t;
    sign * (t - numerator / denominator)
}

#[derive(Debug, Serialize)]
struct MetricEffect {
    baseline_samples: usize,
    candidate_samples: usize,
    baseline: Option<DistributionSummary>,
    candidate: Option<DistributionSummary>,
    improvement_pct: Option<f64>,
    ci_pct: Option<[f64; 2]>,
    minimum_effect_pct: f64,
    guardrail_regressed: bool,
    classification: Classification,
    baseline_value: Option<f64>,
    candidate_value: Option<f64>,
}

impl MetricEffect {
    fn insufficient(minimum_effect_pct: f64) -> Self {
        Self {
            baseline_samples: 0,
            candidate_samples: 0,
            baseline: None,
            candidate: None,
            improvement_pct: None,
            ci_pct: None,
            minimum_effect_pct,
            guardrail_regressed: false,
            classification: Classification::Inconclusive,
            baseline_value: None,
            candidate_value: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct DistributionSummary {
    count: usize,
    mean: f64,
    standard_deviation: f64,
    median: f64,
    q1: f64,
    q3: f64,
}

impl DistributionSummary {
    fn new(values: &[f64]) -> Self {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let mean = mean(values);
        let variance = if values.len() > 1 {
            values
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / (values.len() - 1) as f64
        } else {
            0.0
        };
        Self {
            count: values.len(),
            mean,
            standard_deviation: variance.sqrt(),
            median: quantile_sorted(&sorted, 0.5),
            q1: quantile_sorted(&sorted, 0.25),
            q3: quantile_sorted(&sorted, 0.75),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Gate {
    Pass,
    Fail,
    Inconclusive,
}

impl Gate {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Inconclusive => "inconclusive",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Classification {
    Improved,
    Regressed,
    Equivalent,
    Inconclusive,
}

impl Classification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Improved => "improved",
            Self::Regressed => "regressed",
            Self::Equivalent => "equivalent",
            Self::Inconclusive => "inconclusive",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Verdict {
    Positive,
    Negative,
    Equivalent,
    Inconclusive,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Positive => "positive",
            Self::Negative => "negative",
            Self::Equivalent => "equivalent",
            Self::Inconclusive => "inconclusive",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bperf_measurement::schedule::TrialPhase;

    fn policy() -> AnalysisPolicy {
        AnalysisPolicy {
            confidence: 0.95,
            bootstrap_samples: 1_000,
            primary_metrics: vec![MetricPolicy {
                name: "workload.wall_ms".to_owned(),
                minimum_effect_pct: 5.0,
            }],
            minimum_success_rate: 0.95,
            max_regression_percentage_points: 1.0,
            protected_metric_max_regression_pct: 3.0,
        }
    }

    fn result(workload: &str, success: bool, value: f64, sample_index: u32) -> TrialResult {
        TrialResult {
            schema_version: bperf_measurement::MEASUREMENT_SCHEMA_VERSION,
            measurement_set_id: "measurement".to_owned(),
            trial_id: format!("{workload}-{sample_index}"),
            attempt: 1,
            workload_id: workload.to_owned(),
            engine: Engine::Chromium,
            phase: TrialPhase::Final,
            sample_index,
            environment_fingerprint: "environment".to_owned(),
            valid: true,
            success,
            failure_category: (!success).then(|| "workload_failed".to_owned()),
            failure_detail: None,
            invalidation_reason: None,
            metrics: BTreeMap::from([("workload.wall_ms".to_owned(), value)]),
            artifacts: Vec::new(),
        }
    }

    fn refs(results: &[TrialResult]) -> Vec<&TrialResult> {
        results.iter().collect()
    }

    #[test]
    fn independent_samples_detect_a_ten_percent_gain() {
        let baseline: Vec<_> = (1..=20)
            .map(|index| result("workload", true, 100.0, index))
            .collect();
        let candidate: Vec<_> = (1..=20)
            .map(|index| result("workload", true, 90.0, index))
            .collect();
        let policy = policy();
        let effect = metric_effect(
            &["workload"],
            &refs(&baseline),
            &refs(&candidate),
            &policy.primary_metrics[0],
            &policy,
            7,
        );
        assert_eq!(effect.classification, Classification::Improved);
        assert!((effect.improvement_pct.unwrap() - 10.0).abs() < 1e-9);
        assert!((effect.baseline_value.unwrap() - 100.0).abs() < 1e-9);
        assert!((effect.candidate_value.unwrap() - 90.0).abs() < 1e-9);
        assert_eq!(effect.baseline_samples, 20);
        assert_eq!(effect.candidate_samples, 20);
    }

    #[test]
    fn equal_independent_samples_are_equivalent() {
        let baseline: Vec<_> = (1..=20)
            .map(|index| result("workload", true, 100.0, index))
            .collect();
        let candidate = baseline.clone();
        let policy = policy();
        let effect = metric_effect(
            &["workload"],
            &refs(&baseline),
            &refs(&candidate),
            &policy.primary_metrics[0],
            &policy,
            7,
        );
        assert_eq!(effect.classification, Classification::Equivalent);
    }

    #[test]
    fn correctness_failure_overrides_faster_measurements() {
        let baseline: Vec<_> = (1..=20)
            .map(|index| result("workload", true, 100.0, index))
            .collect();
        let candidate: Vec<_> = (1..=20)
            .map(|index| result("workload", false, 50.0, index))
            .collect();
        let analysis = correctness(
            &["workload"],
            &refs(&baseline),
            &refs(&candidate),
            true,
            &policy(),
            9,
        );
        assert_eq!(analysis.gate, Gate::Fail);
    }

    #[test]
    fn independent_bootstrap_is_deterministic() {
        let left = BTreeMap::from([
            ("a".to_owned(), vec![1.0, 2.0, 3.0]),
            ("b".to_owned(), vec![10.0, 11.0, 12.0]),
        ]);
        let right = BTreeMap::from([
            ("a".to_owned(), vec![0.5, 1.5, 2.5]),
            ("b".to_owned(), vec![9.0, 10.0, 11.0]),
        ]);
        assert_eq!(
            independent_bootstrap_interval(&["a", "b"], &left, &right, 0.95, 1_000, 42),
            independent_bootstrap_interval(&["a", "b"], &left, &right, 0.95, 1_000, 42)
        );
    }

    #[test]
    fn wilson_interval_and_normal_quantile_are_sensible() {
        assert!((inverse_normal(0.975) - 1.96).abs() < 0.001);
        let rate = SuccessRate::new(95, 100, 0.95);
        let interval = rate.wilson_ci_pct.unwrap();
        assert!(interval[0] < 95.0 && interval[1] > 95.0);
        assert!(interval[0] > 85.0 && interval[1] < 100.0);
    }

    #[test]
    fn runtime_anchors_accept_stable_hosts() {
        let baseline = [
            9.9, 10.0, 10.1, 10.0, 9.9, 10.1, 10.0, 10.0, 10.1, 9.9, 10.0, 10.1, 9.9, 10.0, 10.1,
        ];
        let candidate = [
            10.0, 10.1, 9.9, 10.0, 10.1, 9.9, 10.0, 10.1, 10.0, 9.9, 10.1, 10.0, 9.9, 10.0, 10.1,
        ];
        let analysis = analyze_anchor(Engine::Chromium, &baseline, &candidate, 0.95, 11);
        assert_eq!(analysis.status, AnchorStatus::Stable);
    }

    #[test]
    fn runtime_anchors_detect_material_drift() {
        let baseline = [10.0; 15];
        let candidate = [12.0; 15];
        let analysis = analyze_anchor(Engine::Firefox, &baseline, &candidate, 0.95, 13);
        assert_eq!(analysis.status, AnchorStatus::Drifted);
        assert!((analysis.drift_pct.unwrap() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn metric_values_choose_one_readable_unit_for_each_pair() {
        for (metric, baseline, candidate, expected) in [
            ("workload.wall_ms", 0.000_25, 0.000_125, "250ns -> 125ns"),
            ("workload.wall_ms", 0.25, 0.125, "250us -> 125us"),
            ("workload.wall_ms", 100.0, 47.55, "100ms -> 47.55ms"),
            ("workload.wall_ms", 1_500.0, 750.0, "1.5s -> 0.75s"),
            ("workload.wall_ms", 0.9, 1.1, "0.9ms -> 1.1ms"),
            ("browser.js_heap.live_bytes", 512.0, 256.0, "512b -> 256b"),
            ("browser.js_heap.live_bytes", 2_048.0, 1_024.0, "2kb -> 1kb"),
        ] {
            assert_eq!(format_metric_values(metric, baseline, candidate), expected);
        }
    }

    #[test]
    fn decision_summary_contains_only_decision_relevant_engine_evidence() {
        let summary = ComparisonSummary {
            comparison_id: "compare-candidate".to_owned(),
            report_path: None,
            baseline_measurement_set: "baseline".to_owned(),
            candidate_measurement_set: "candidate".to_owned(),
            environment_fingerprint: Some("environment".to_owned()),
            policy: "strict_all".to_owned(),
            verdict: "negative".to_owned(),
            engines: vec![EngineSummary {
                engine: Engine::Webkit,
                verdict: "negative".to_owned(),
                correctness: "pass".to_owned(),
                anchor: Some(AnchorSummary {
                    status: "stable".to_owned(),
                    drift_pct: Some(0.25),
                    ci_pct: Some([-0.5, 1.0]),
                }),
                metrics: BTreeMap::from([(
                    "workload.wall_ms".to_owned(),
                    MetricSummary {
                        improvement_pct: Some(-4.5),
                        ci_pct: Some([-6.0, -3.0]),
                        classification: "regressed".to_owned(),
                        guardrail_regressed: true,
                        baseline_value: Some(100.0),
                        candidate_value: Some(104.5),
                    },
                )]),
            }],
            warnings: vec!["baseline is old".to_owned()],
        };

        assert_eq!(
            summary.render_decision_summary(),
            "  webkit: negative correctness=pass anchor=stable (+0.25%)\n\
             \x20   workload.wall_ms: regressed effect=-4.50% (100ms -> 104.5ms) ci=[-6.00%, -3.00%] guardrail=regressed\n\
             \x20 warning: baseline is old\n"
        );
    }
}
