//! Budgeted final-sample selection from calibration evidence.

use std::{cmp::Ordering, collections::HashMap, str::FromStr};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::Engine;
use serde::{Deserialize, Serialize};

use crate::{
    MEASUREMENT_SCHEMA_VERSION,
    manifest::AnalysisPolicy,
    schedule::{MeasurementSchedule, SamplingSchedule, TrialPhase},
    store::TrialResult,
};

pub const TRIAL_ELAPSED_METRIC: &str = "bperf.trial.elapsed_ms";
#[cfg(test)]
pub(crate) const CAPTURE_ELAPSED_METRIC: &str = "bperf.capture.elapsed_ms";
pub(crate) const BATCH_SIZE_METRIC: &str = "bperf.batch_size";
pub(crate) const PROFILE_BATCH_TARGET_MS: f64 = 100.0;
pub(crate) const MAX_BATCH_SIZE: u32 = 10_000;
const MIN_ADAPTIVE_PILOTS: u32 = 5;
const STABILITY_PREFIXES: usize = 3;
const SAMPLE_REQUIREMENT_TOLERANCE: f64 = 0.10;
const MEDIAN_ESTIMATE_TOLERANCE: f64 = 0.20;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PilotStopReason {
    Stable,
    MaximumSamples,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunBudget {
    milliseconds: u64,
}

impl RunBudget {
    pub(crate) const fn milliseconds(self) -> u64 {
        self.milliseconds
    }
}

impl FromStr for RunBudget {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let value = value.trim();
        let number_end = value
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(value.len());
        if number_end == 0 || number_end == value.len() {
            return Err("use an integer duration with ms, s, m, or h (for example, 5m)".into());
        }
        let amount: u64 = value[..number_end]
            .parse()
            .map_err(|_| format!("invalid duration {value:?}"))?;
        if amount == 0 {
            return Err("budget must be greater than zero".into());
        }
        let multiplier = match &value[number_end..] {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => {
                return Err("use an integer duration with ms, s, m, or h (for example, 5m)".into());
            }
        };
        let milliseconds = amount
            .checked_mul(multiplier)
            .ok_or_else(|| "budget is too large".to_owned())?;
        Ok(Self { milliseconds })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingDecision {
    pub schema_version: u32,
    pub budget_ms: u64,
    pub calibration_elapsed_ms: f64,
    pub estimated_total_ms: f64,
    pub budget_limited: bool,
    pub estimated_budget_overrun: bool,
    pub required_final_trials: u32,
    pub selected_final_trials: u32,
    pub strata: Vec<StratumDecision>,
}

impl SamplingDecision {
    pub fn final_samples_for(&self, workload_id: &str, engine: Engine) -> Option<u32> {
        self.strata
            .iter()
            .find(|stratum| stratum.workload_id == workload_id && stratum.engine == engine)
            .map(|stratum| stratum.final_samples)
    }

    pub fn batch_size_for(&self, workload_id: &str, engine: Engine) -> Option<u32> {
        self.strata
            .iter()
            .find(|stratum| stratum.workload_id == workload_id && stratum.engine == engine)
            .map(|stratum| stratum.batch_size)
    }

    pub fn pilot_samples_for(&self, workload_id: &str, engine: Engine) -> Option<u32> {
        self.strata
            .iter()
            .find(|stratum| stratum.workload_id == workload_id && stratum.engine == engine)
            .map(|stratum| stratum.pilot_samples)
    }

    pub(crate) fn validate(&self, schedule: &MeasurementSchedule) -> Result<()> {
        if self.schema_version != MEASUREMENT_SCHEMA_VERSION {
            bail!(
                "sampling decision uses unsupported schema {}",
                self.schema_version
            );
        }
        let SamplingSchedule::Adaptive {
            budget_ms,
            min_final_samples,
        } = schedule.sampling
        else {
            bail!("a fixed schedule cannot have an adaptive sampling decision");
        };
        if self.budget_ms != budget_ms {
            bail!("sampling decision budget does not match its schedule");
        }
        for (name, value) in [
            ("calibration_elapsed_ms", self.calibration_elapsed_ms),
            ("estimated_total_ms", self.estimated_total_ms),
        ] {
            if !value.is_finite() || value < 0.0 {
                bail!("sampling decision {name} must be finite and non-negative");
            }
        }
        if self.estimated_total_ms < self.calibration_elapsed_ms {
            bail!("sampling decision total time is shorter than calibration");
        }

        let mut expected = HashMap::new();
        for trial in schedule
            .trials
            .iter()
            .filter(|trial| trial.phase == TrialPhase::Final)
        {
            expected
                .entry((trial.workload_id.as_str(), trial.engine))
                .or_insert(0_u32);
        }
        let mut expected_pilots = HashMap::new();
        for trial in schedule
            .trials
            .iter()
            .filter(|trial| trial.phase == TrialPhase::Pilot)
        {
            *expected_pilots
                .entry((trial.workload_id.as_str(), trial.engine))
                .or_insert(0_u32) += 1;
        }
        if self.strata.len() != expected.len() {
            bail!("sampling decision does not cover every workload and engine");
        }

        let mut selected_total = 0_u32;
        let mut required_total = 0_u32;
        let mut budget_limited = false;
        for stratum in &self.strata {
            let key = (stratum.workload_id.as_str(), stratum.engine);
            let Some(marker) = expected.get_mut(&key) else {
                bail!(
                    "sampling decision contains unknown stratum {}/{}",
                    stratum.workload_id,
                    stratum.engine
                );
            };
            if *marker != 0 {
                bail!(
                    "sampling decision repeats stratum {}/{}",
                    stratum.workload_id,
                    stratum.engine
                );
            }
            *marker = 1;
            let Some(maximum_pilots) = expected_pilots.get(&key).copied() else {
                bail!(
                    "sampling decision has no pilot envelope for {}/{}",
                    stratum.workload_id,
                    stratum.engine
                );
            };
            let minimum_pilots = minimum_pilot_samples(maximum_pilots);
            if !(minimum_pilots..=maximum_pilots).contains(&stratum.pilot_samples)
                || (stratum.pilot_stop_reason == PilotStopReason::MaximumSamples
                    && stratum.pilot_samples != maximum_pilots)
            {
                bail!(
                    "sampling decision has an invalid pilot count for {}/{}",
                    stratum.workload_id,
                    stratum.engine
                );
            }
            if !stratum.estimated_trial_ms.is_finite()
                || stratum.estimated_trial_ms <= 0.0
                || stratum.batch_size == 0
                || stratum.batch_size > MAX_BATCH_SIZE
            {
                bail!(
                    "sampling decision has invalid trial evidence for {}/{}",
                    stratum.workload_id,
                    stratum.engine
                );
            }
            if !(min_final_samples..=schedule.final_samples).contains(&stratum.final_samples)
                || !(min_final_samples..=schedule.final_samples)
                    .contains(&stratum.required_final_samples)
                || stratum.final_samples > stratum.required_final_samples
            {
                bail!(
                    "sampling decision has an invalid final count for {}/{}",
                    stratum.workload_id,
                    stratum.engine
                );
            }
            budget_limited |= stratum.final_samples < stratum.required_final_samples;
            for metric in &stratum.metrics {
                if metric.observations != stratum.pilot_samples
                    || !metric.target_relative_margin_pct.is_finite()
                    || metric.target_relative_margin_pct < 0.0
                    || !(min_final_samples..=schedule.final_samples)
                        .contains(&metric.required_samples)
                    || metric
                        .log_standard_deviation
                        .is_some_and(|value| !value.is_finite() || value < 0.0)
                    || metric
                        .selected_relative_margin_pct
                        .is_some_and(|value| !value.is_finite() || value < 0.0)
                {
                    bail!(
                        "sampling decision has invalid metric evidence for {}/{}",
                        stratum.workload_id,
                        stratum.engine
                    );
                }
            }
            selected_total = selected_total
                .checked_add(stratum.final_samples)
                .context("selected final trial count overflowed")?;
            required_total = required_total
                .checked_add(stratum.required_final_samples)
                .context("required final trial count overflowed")?;
        }
        if expected.values().any(|marker| *marker == 0) {
            bail!("sampling decision does not cover every workload and engine");
        }
        if selected_total != self.selected_final_trials
            || required_total != self.required_final_trials
        {
            bail!("sampling decision aggregate counts are inconsistent");
        }
        if self.budget_limited != budget_limited
            || self.estimated_budget_overrun != (self.estimated_total_ms > self.budget_ms as f64)
        {
            bail!("sampling decision budget flags are inconsistent");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StratumDecision {
    pub workload_id: String,
    pub engine: Engine,
    pub pilot_samples: u32,
    pub pilot_stop_reason: PilotStopReason,
    #[serde(default = "single_batch")]
    pub batch_size: u32,
    pub estimated_trial_ms: f64,
    pub required_final_samples: u32,
    pub final_samples: u32,
    pub metrics: Vec<MetricEstimate>,
}

const fn single_batch() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricEstimate {
    pub metric: String,
    pub observations: u32,
    pub target_relative_margin_pct: f64,
    pub log_standard_deviation: Option<f64>,
    pub required_samples: u32,
    pub selected_relative_margin_pct: Option<f64>,
}

pub(crate) fn pilot_stop_reason(
    schedule: &MeasurementSchedule,
    policy: &AnalysisPolicy,
    workload_id: &str,
    engine: Engine,
    pilots: &[&TrialResult],
) -> Option<PilotStopReason> {
    let maximum = schedule
        .trials
        .iter()
        .filter(|trial| {
            trial.phase == TrialPhase::Pilot
                && trial.workload_id == workload_id
                && trial.engine == engine
        })
        .count() as u32;
    let observations = pilots.len() as u32;
    if maximum == 0 || observations < minimum_pilot_samples(maximum) {
        return None;
    }
    if pilot_estimates_are_stable(schedule, policy, pilots) {
        Some(PilotStopReason::Stable)
    } else if observations >= maximum {
        Some(PilotStopReason::MaximumSamples)
    } else {
        None
    }
}

const fn minimum_pilot_samples(maximum: u32) -> u32 {
    if maximum < MIN_ADAPTIVE_PILOTS {
        maximum
    } else {
        MIN_ADAPTIVE_PILOTS
    }
}

fn pilot_estimates_are_stable(
    schedule: &MeasurementSchedule,
    policy: &AnalysisPolicy,
    pilots: &[&TrialResult],
) -> bool {
    if pilots.len() < STABILITY_PREFIXES {
        return false;
    }
    let SamplingSchedule::Adaptive {
        min_final_samples, ..
    } = schedule.sampling
    else {
        return false;
    };
    let first_prefix = pilots.len() + 1 - STABILITY_PREFIXES;
    let confidence_multiplier = inverse_normal_cdf((1.0 + policy.confidence) / 2.0);

    for metric in &policy.primary_metrics {
        let requirements = (first_prefix..=pilots.len())
            .map(|prefix| {
                let values: Vec<_> = pilots[..prefix]
                    .iter()
                    .filter_map(|result| result.metrics.get(&metric.name).copied())
                    .filter(|value| *value > 0.0)
                    .collect();
                required_samples(
                    log_standard_deviation(&values),
                    confidence_multiplier,
                    metric.minimum_effect_pct,
                    min_final_samples,
                    schedule.final_samples,
                )
            })
            .collect::<Vec<_>>();
        if !sample_requirements_are_stable(&requirements) {
            return false;
        }
    }

    [BATCH_SIZE_METRIC, TRIAL_ELAPSED_METRIC]
        .into_iter()
        .all(|metric| {
            let estimates = (first_prefix..=pilots.len())
                .filter_map(|prefix| median_metric(&pilots[..prefix], metric).ok())
                .collect::<Vec<_>>();
            estimates.len() == STABILITY_PREFIXES
                && relative_spread(&estimates) <= MEDIAN_ESTIMATE_TOLERANCE
        })
}

fn sample_requirements_are_stable(requirements: &[u32]) -> bool {
    let Some(latest) = requirements.last().copied() else {
        return false;
    };
    let minimum = requirements.iter().copied().min().unwrap_or(latest);
    let maximum = requirements.iter().copied().max().unwrap_or(latest);
    let tolerance = (f64::from(latest) * SAMPLE_REQUIREMENT_TOLERANCE)
        .ceil()
        .max(2.0) as u32;
    maximum - minimum <= tolerance
}

fn relative_spread(values: &[f64]) -> f64 {
    let minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if !minimum.is_finite() || !maximum.is_finite() || maximum <= 0.0 {
        return f64::INFINITY;
    }
    (maximum - minimum) / maximum
}

pub fn decide(
    schedule: &MeasurementSchedule,
    policy: &AnalysisPolicy,
    calibration_results: &[&TrialResult],
) -> Result<SamplingDecision> {
    let SamplingSchedule::Adaptive {
        budget_ms,
        min_final_samples,
    } = schedule.sampling
    else {
        bail!("cannot calibrate a fixed measurement schedule");
    };
    let expected_warmups = schedule
        .trials
        .iter()
        .filter(|trial| trial.phase == TrialPhase::Warmup)
        .map(|trial| trial.trial_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let completed_warmups = calibration_results
        .iter()
        .filter(|result| result.phase == TrialPhase::Warmup)
        .map(|result| result.trial_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    if completed_warmups != expected_warmups {
        bail!("adaptive sampling needs every configured warmup trial");
    }

    let calibration_elapsed_ms = calibration_results
        .iter()
        .map(|result| elapsed_metric(result))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum();
    let mut pilot_results: HashMap<(&str, Engine), Vec<&TrialResult>> = HashMap::new();
    for result in calibration_results
        .iter()
        .copied()
        .filter(|result| result.phase == TrialPhase::Pilot)
    {
        pilot_results
            .entry((&result.workload_id, result.engine))
            .or_default()
            .push(result);
    }
    for pilots in pilot_results.values_mut() {
        pilots.sort_by_key(|result| result.sample_index);
    }

    let mut keys: Vec<_> = schedule
        .trials
        .iter()
        .filter(|trial| trial.phase == TrialPhase::Final)
        .map(|trial| (trial.workload_id.as_str(), trial.engine))
        .collect();
    keys.sort_by(|left, right| {
        left.0
            .cmp(right.0)
            .then_with(|| left.1.as_str().cmp(right.1.as_str()))
    });
    keys.dedup();

    let confidence_multiplier = inverse_normal_cdf((1.0 + policy.confidence) / 2.0);
    let mut strata = Vec::with_capacity(keys.len());
    for (workload_id, engine) in keys {
        let pilots = pilot_results
            .get(&(workload_id, engine))
            .with_context(|| format!("no pilot results for {workload_id}/{engine}"))?;
        let pilot_stop_reason = pilot_stop_reason(
            schedule,
            policy,
            workload_id,
            engine,
            pilots,
        )
        .with_context(|| {
            format!(
                "pilot evidence for {workload_id}/{engine} has not stabilized or reached its cap"
            )
        })?;
        let elapsed = pilots
            .iter()
            .map(|result| elapsed_metric(result))
            .collect::<Result<Vec<_>>>()?;
        let estimated_trial_ms = median(&elapsed);
        let batch_size = median_metric(pilots, BATCH_SIZE_METRIC)?
            .round()
            .clamp(1.0, f64::from(MAX_BATCH_SIZE)) as u32;
        let mut metrics = Vec::with_capacity(policy.primary_metrics.len());
        for metric in &policy.primary_metrics {
            let values: Vec<_> = pilots
                .iter()
                .filter_map(|result| result.metrics.get(&metric.name).copied())
                .filter(|value| *value > 0.0)
                .collect();
            let log_standard_deviation = log_standard_deviation(&values);
            let required_samples = required_samples(
                log_standard_deviation,
                confidence_multiplier,
                metric.minimum_effect_pct,
                min_final_samples,
                schedule.final_samples,
            );
            metrics.push(MetricEstimate {
                metric: metric.name.clone(),
                observations: values.len() as u32,
                target_relative_margin_pct: metric.minimum_effect_pct,
                log_standard_deviation,
                required_samples,
                selected_relative_margin_pct: None,
            });
        }
        let required_final_samples = metrics
            .iter()
            .map(|metric| metric.required_samples)
            .max()
            .unwrap_or(min_final_samples);
        strata.push(StratumDecision {
            workload_id: workload_id.to_owned(),
            engine,
            pilot_samples: pilots.len() as u32,
            pilot_stop_reason,
            batch_size,
            estimated_trial_ms,
            required_final_samples,
            final_samples: min_final_samples,
            metrics,
        });
    }

    allocate_budget(&mut strata, budget_ms as f64 - calibration_elapsed_ms);
    for stratum in &mut strata {
        for metric in &mut stratum.metrics {
            metric.selected_relative_margin_pct = metric.log_standard_deviation.map(|deviation| {
                relative_margin_pct(confidence_multiplier, deviation, stratum.final_samples)
            });
        }
    }

    let required_final_trials = strata
        .iter()
        .map(|stratum| stratum.required_final_samples)
        .sum();
    let selected_final_trials = strata.iter().map(|stratum| stratum.final_samples).sum();
    let estimated_total_ms = calibration_elapsed_ms
        + strata
            .iter()
            .map(|stratum| stratum.estimated_trial_ms * f64::from(stratum.final_samples))
            .sum::<f64>();
    let decision = SamplingDecision {
        schema_version: MEASUREMENT_SCHEMA_VERSION,
        budget_ms,
        calibration_elapsed_ms,
        estimated_total_ms,
        budget_limited: strata
            .iter()
            .any(|stratum| stratum.final_samples < stratum.required_final_samples),
        estimated_budget_overrun: estimated_total_ms > budget_ms as f64,
        required_final_trials,
        selected_final_trials,
        strata,
    };
    decision.validate(schedule)?;
    Ok(decision)
}

fn elapsed_metric(result: &TrialResult) -> Result<f64> {
    result
        .metrics
        .get(TRIAL_ELAPSED_METRIC)
        .copied()
        .filter(|value| *value > 0.0)
        .with_context(|| {
            format!(
                "calibration trial {} has no positive {TRIAL_ELAPSED_METRIC}",
                result.trial_id
            )
        })
}

fn median_metric(results: &[&TrialResult], metric: &str) -> Result<f64> {
    let values: Vec<_> = results
        .iter()
        .filter_map(|result| result.metrics.get(metric).copied())
        .filter(|value| *value > 0.0)
        .collect();
    if values.is_empty() {
        bail!("pilot results have no positive {metric}");
    }
    Ok(median(&values))
}

fn allocate_budget(strata: &mut [StratumDecision], remaining_budget_ms: f64) {
    let minimum_cost: f64 = strata
        .iter()
        .map(|stratum| stratum.estimated_trial_ms * f64::from(stratum.final_samples))
        .sum();
    let mut available = (remaining_budget_ms - minimum_cost).max(0.0);
    loop {
        let next = strata
            .iter()
            .enumerate()
            .filter(|(_, stratum)| {
                stratum.final_samples < stratum.required_final_samples
                    && stratum.estimated_trial_ms <= available
            })
            .max_by(|(_, left), (_, right)| {
                allocation_priority(left)
                    .partial_cmp(&allocation_priority(right))
                    .unwrap_or(Ordering::Equal)
            })
            .map(|(index, _)| index);
        let Some(index) = next else {
            break;
        };
        strata[index].final_samples += 1;
        available -= strata[index].estimated_trial_ms;
    }
}

fn allocation_priority(stratum: &StratumDecision) -> f64 {
    let remaining = stratum.required_final_samples - stratum.final_samples;
    f64::from(remaining) / f64::from(stratum.required_final_samples) / stratum.estimated_trial_ms
}

fn required_samples(
    log_standard_deviation: Option<f64>,
    confidence_multiplier: f64,
    target_relative_margin_pct: f64,
    minimum: u32,
    maximum: u32,
) -> u32 {
    let Some(deviation) = log_standard_deviation else {
        return maximum;
    };
    if deviation == 0.0 {
        return minimum;
    }
    if target_relative_margin_pct <= 0.0 {
        return maximum;
    }
    let target_log_margin = (1.0 + target_relative_margin_pct / 100.0).ln();
    let estimate = ((confidence_multiplier * deviation) / target_log_margin).powi(2);
    if !estimate.is_finite() || estimate >= f64::from(maximum) {
        maximum
    } else {
        (estimate.ceil() as u32).clamp(minimum, maximum)
    }
}

fn relative_margin_pct(confidence_multiplier: f64, deviation: f64, samples: u32) -> f64 {
    ((confidence_multiplier * deviation / f64::from(samples).sqrt()).exp() - 1.0) * 100.0
}

fn log_standard_deviation(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let logs: Vec<_> = values.iter().map(|value| value.ln()).collect();
    let mean = logs.iter().sum::<f64>() / logs.len() as f64;
    let variance =
        logs.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / (logs.len() - 1) as f64;
    Some(variance.sqrt())
}

fn median(values: &[f64]) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn inverse_normal_cdf(probability: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    const LOWER: f64 = 0.024_25;
    const UPPER: f64 = 1.0 - LOWER;

    if probability < LOWER {
        let q = (-2.0 * probability.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if probability <= UPPER {
        let q = probability - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - probability).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        manifest::{AnalysisPolicy, MetricPolicy},
        schedule::ScheduledTrial,
    };

    #[test]
    fn parses_explicit_human_durations() {
        assert_eq!("250ms".parse::<RunBudget>().unwrap().milliseconds(), 250);
        assert_eq!("45s".parse::<RunBudget>().unwrap().milliseconds(), 45_000);
        assert_eq!("5m".parse::<RunBudget>().unwrap().milliseconds(), 300_000);
        assert_eq!("2h".parse::<RunBudget>().unwrap().milliseconds(), 7_200_000);
        assert!("5".parse::<RunBudget>().is_err());
        assert!("0s".parse::<RunBudget>().is_err());
    }

    #[test]
    fn stable_pilots_need_fewer_samples_than_noisy_pilots() {
        let stable = log_standard_deviation(&[99.0, 100.0, 100.5, 100.0]).unwrap();
        let noisy = log_standard_deviation(&[50.0, 100.0, 200.0, 400.0]).unwrap();
        let z = inverse_normal_cdf(0.975);
        assert_eq!(required_samples(Some(stable), z, 5.0, 2, 100), 2);
        assert_eq!(required_samples(Some(noisy), z, 5.0, 2, 100), 100);
        assert!((z - 1.959_963_986).abs() < 1e-6);
    }

    #[test]
    fn stable_calibration_stops_at_the_minimum_pilot_prefix() {
        let schedule = schedule_with_pilots(10, 20, 100, 1_000_000);
        let results = [99.0, 100.0, 100.5, 100.0, 99.5]
            .into_iter()
            .enumerate()
            .map(|(index, value)| pilot_result(Engine::Chromium, index as u32 + 1, value))
            .collect::<Vec<_>>();
        let first_four = results[..4].iter().collect::<Vec<_>>();
        let first_five = results.iter().collect::<Vec<_>>();

        assert_eq!(
            pilot_stop_reason(&schedule, &policy(), "case", Engine::Chromium, &first_four,),
            None
        );
        assert_eq!(
            pilot_stop_reason(&schedule, &policy(), "case", Engine::Chromium, &first_five,),
            Some(PilotStopReason::Stable)
        );
    }

    #[test]
    fn unstable_calibration_uses_its_pilot_cap() {
        let schedule = schedule_with_pilots(5, 20, 100, 1_000_000);
        let results = [100.0, 100.0, 100.0, 100.0, 400.0]
            .into_iter()
            .enumerate()
            .map(|(index, value)| pilot_result(Engine::Chromium, index as u32 + 1, value))
            .collect::<Vec<_>>();
        let pilots = results.iter().collect::<Vec<_>>();

        assert_eq!(
            pilot_stop_reason(&schedule, &policy(), "case", Engine::Chromium, &pilots,),
            Some(PilotStopReason::MaximumSamples)
        );
    }

    #[test]
    fn budget_allocation_preserves_each_stratum_minimum() {
        let mut strata = vec![
            stratum("case", Engine::Chromium, 100.0, 10),
            stratum("case", Engine::Firefox, 100.0, 10),
            stratum("case", Engine::Webkit, 100.0, 10),
        ];
        allocate_budget(&mut strata, 650.0);
        assert_eq!(
            strata
                .iter()
                .map(|stratum| stratum.final_samples)
                .sum::<u32>(),
            6
        );
        assert!(strata.iter().all(|stratum| stratum.final_samples >= 2));
    }

    #[test]
    fn calibration_sizes_each_browser_independently() {
        let schedule = schedule_with_pilots(4, 2, 100, 1_000_000);
        let mut results = Vec::new();
        for (engine, values) in [
            (Engine::Chromium, [99.0, 100.0, 100.5, 100.0]),
            (Engine::Firefox, [50.0, 100.0, 200.0, 400.0]),
            (Engine::Webkit, [90.0, 110.0, 90.0, 110.0]),
        ] {
            for (index, value) in values.into_iter().enumerate() {
                results.push(pilot_result(engine, index as u32 + 1, value));
            }
        }
        let references = results.iter().collect::<Vec<_>>();
        let decision = decide(&schedule, &policy(), &references).unwrap();

        let chromium = decision
            .final_samples_for("case", Engine::Chromium)
            .unwrap();
        let firefox = decision.final_samples_for("case", Engine::Firefox).unwrap();
        let webkit = decision.final_samples_for("case", Engine::Webkit).unwrap();
        assert!(chromium < webkit);
        assert!(webkit < firefox);
        assert!(!decision.budget_limited);
        assert_eq!(decision.selected_final_trials, chromium + firefox + webkit);
        for stratum in &decision.strata {
            assert_eq!(stratum.batch_size, 1);
        }
    }

    #[test]
    fn the_noisiest_metric_sets_the_combined_trial_count() {
        let schedule = schedule_with_pilots(4, 2, 100, 10_000_000);
        let mut results = Vec::new();
        for engine in Engine::ALL {
            for (index, cpu) in [50.0, 100.0, 200.0, 400.0].into_iter().enumerate() {
                results.push(pilot_result_metrics(
                    engine,
                    index as u32 + 1,
                    100.0,
                    cpu,
                    1_024.0,
                ));
            }
        }
        let references = results.iter().collect::<Vec<_>>();
        let decision = decide(&schedule, &all_metrics_policy(), &references).unwrap();

        for stratum in &decision.strata {
            assert_eq!(stratum.final_samples, 100);
        }
        assert_eq!(decision.selected_final_trials, 300);
    }

    fn stratum(
        workload_id: &str,
        engine: Engine,
        estimated_trial_ms: f64,
        required_final_samples: u32,
    ) -> StratumDecision {
        StratumDecision {
            workload_id: workload_id.to_owned(),
            engine,
            pilot_samples: 4,
            pilot_stop_reason: PilotStopReason::MaximumSamples,
            batch_size: 1,
            estimated_trial_ms,
            required_final_samples,
            final_samples: 2,
            metrics: Vec::new(),
        }
    }

    fn schedule_with_pilots(
        pilot_samples: u32,
        minimum: u32,
        maximum: u32,
        budget_ms: u64,
    ) -> MeasurementSchedule {
        let mut trials = Vec::new();
        for engine in Engine::ALL {
            for sample_index in 1..=pilot_samples {
                trials.push(scheduled(engine, TrialPhase::Pilot, sample_index));
            }
            for sample_index in 1..=maximum {
                trials.push(scheduled(engine, TrialPhase::Final, sample_index));
            }
        }
        for (index, trial) in trials.iter_mut().enumerate() {
            trial.sequence = index as u32 + 1;
        }
        MeasurementSchedule {
            schema_version: MEASUREMENT_SCHEMA_VERSION,
            measurement_set_id: "measure-test".into(),
            benchmark_id: "benchmark".into(),
            subject_id: "subject".into(),
            benchmark_sha256: "benchmark-sha".into(),
            variant_id: "variant".into(),
            variant_sha256: "variant-sha".into(),
            schedule_seed: 7,
            final_samples: maximum,
            cohort: None,
            sampling: SamplingSchedule::Adaptive {
                budget_ms,
                min_final_samples: minimum,
            },
            trials,
        }
    }

    fn scheduled(engine: Engine, phase: TrialPhase, sample_index: u32) -> ScheduledTrial {
        ScheduledTrial {
            trial_id: format!(
                "{}-case-{}-{sample_index:04}",
                match phase {
                    TrialPhase::Warmup => "warmup",
                    TrialPhase::Pilot => "pilot",
                    TrialPhase::Final => "final",
                },
                engine.as_str(),
            ),
            sequence: 0,
            workload_id: "case".into(),
            engine,
            phase,
            sample_index,
        }
    }

    fn pilot_result(engine: Engine, sample_index: u32, value: f64) -> TrialResult {
        pilot_result_metrics(engine, sample_index, value, value, 1_024.0)
    }

    fn pilot_result_metrics(
        engine: Engine,
        sample_index: u32,
        timing: f64,
        cpu: f64,
        heap: f64,
    ) -> TrialResult {
        TrialResult {
            schema_version: MEASUREMENT_SCHEMA_VERSION,
            measurement_set_id: "measure-test".into(),
            trial_id: format!("pilot-case-{}-{sample_index:04}", engine.as_str()),
            attempt: 1,
            workload_id: "case".into(),
            engine,
            phase: TrialPhase::Pilot,
            sample_index,
            environment_fingerprint: "environment".into(),
            valid: true,
            success: true,
            failure_category: None,
            failure_detail: None,
            invalidation_reason: None,
            metrics: BTreeMap::from([
                ("workload.wall_ms".into(), timing),
                ("variant.call_wall_ms".into(), timing / 2.0),
                ("browser.cpu_profile.active_ms".into(), cpu),
                ("browser.js_heap.live_bytes".into(), heap),
                (CAPTURE_ELAPSED_METRIC.into(), 60.0),
                (BATCH_SIZE_METRIC.into(), 1.0),
                (TRIAL_ELAPSED_METRIC.into(), 60.0),
            ]),
            artifacts: Vec::new(),
        }
    }

    fn policy() -> AnalysisPolicy {
        AnalysisPolicy {
            confidence: 0.95,
            bootstrap_samples: 1_000,
            primary_metrics: vec![MetricPolicy {
                name: "workload.wall_ms".into(),
                minimum_effect_pct: 5.0,
            }],
            minimum_success_rate: 0.95,
            max_regression_percentage_points: 1.0,
            protected_metric_max_regression_pct: 3.0,
        }
    }

    fn all_metrics_policy() -> AnalysisPolicy {
        AnalysisPolicy {
            primary_metrics: [
                "workload.wall_ms",
                "browser.cpu_profile.active_ms",
                "browser.js_heap.live_bytes",
            ]
            .into_iter()
            .map(|name| MetricPolicy {
                name: name.into(),
                minimum_effect_pct: 5.0,
            })
            .collect(),
            ..policy()
        }
    }
}
