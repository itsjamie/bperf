//! Deterministic scheduling for one variant measurement set.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bperf_browser::lab::Engine;

use crate::{
    MEASUREMENT_SCHEMA_VERSION,
    manifest::{BenchmarkManifest, VariantDescriptor},
};

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementSchedule {
    pub schema_version: u32,
    pub measurement_set_id: String,
    pub benchmark_id: String,
    pub subject_id: String,
    pub benchmark_sha256: String,
    pub variant_id: String,
    pub variant_sha256: String,
    pub schedule_seed: u64,
    pub final_samples: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cohort: Option<String>,
    #[serde(default)]
    pub sampling: SamplingSchedule,
    pub trials: Vec<ScheduledTrial>,
}

impl MeasurementSchedule {
    pub fn build(
        benchmark: &BenchmarkManifest,
        variant: &VariantDescriptor,
        measurement_set_id: String,
        final_samples: u32,
    ) -> Self {
        Self::build_with_sampling(
            benchmark,
            variant,
            measurement_set_id,
            final_samples,
            None,
            SamplingSchedule::Fixed,
        )
    }

    pub fn build_adaptive(
        benchmark: &BenchmarkManifest,
        variant: &VariantDescriptor,
        measurement_set_id: String,
        budget_ms: u64,
        min_final_samples: u32,
        max_final_samples: u32,
        cohort: Option<String>,
    ) -> Self {
        Self::build_with_sampling(
            benchmark,
            variant,
            measurement_set_id,
            max_final_samples,
            cohort,
            SamplingSchedule::Adaptive {
                budget_ms,
                min_final_samples,
            },
        )
    }

    fn build_with_sampling(
        benchmark: &BenchmarkManifest,
        variant: &VariantDescriptor,
        measurement_set_id: String,
        final_samples: u32,
        cohort: Option<String>,
        sampling: SamplingSchedule,
    ) -> Self {
        let workloads: Vec<_> = benchmark.workload_ids().collect();
        let mut trials = Vec::new();
        for (phase, count) in [
            (TrialPhase::Warmup, benchmark.warmup_samples()),
            (TrialPhase::Pilot, benchmark.pilot_samples()),
            (TrialPhase::Final, final_samples),
        ] {
            trials.extend(phase_trials(
                benchmark.schedule_seed(),
                benchmark.randomize_order(),
                &workloads,
                benchmark.engines(),
                phase,
                count,
            ));
        }
        for (index, trial) in trials.iter_mut().enumerate() {
            trial.sequence = index as u32 + 1;
        }

        Self {
            schema_version: MEASUREMENT_SCHEMA_VERSION,
            measurement_set_id,
            benchmark_id: benchmark.benchmark_id().to_owned(),
            subject_id: benchmark.subject_id().to_owned(),
            benchmark_sha256: benchmark.source_sha256().to_owned(),
            variant_id: variant.id().to_owned(),
            variant_sha256: variant.source_sha256().to_owned(),
            schedule_seed: benchmark.schedule_seed(),
            final_samples,
            cohort,
            sampling,
            trials,
        }
    }

    pub fn final_trial_count(&self) -> usize {
        self.trials
            .iter()
            .filter(|trial| trial.phase == TrialPhase::Final)
            .count()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SamplingSchedule {
    #[default]
    Fixed,
    Adaptive {
        budget_ms: u64,
        min_final_samples: u32,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrialPhase {
    Warmup,
    Pilot,
    Final,
}

impl TrialPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Warmup => "warmup",
            Self::Pilot => "pilot",
            Self::Final => "final",
        }
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledTrial {
    pub trial_id: String,
    pub sequence: u32,
    pub workload_id: String,
    pub engine: Engine,
    pub phase: TrialPhase,
    pub sample_index: u32,
}

fn phase_trials(
    seed: u64,
    randomize_order: bool,
    workloads: &[&str],
    engines: &[Engine],
    phase: TrialPhase,
    count: u32,
) -> Vec<ScheduledTrial> {
    let mut ranked = Vec::new();
    for workload_id in workloads {
        for engine in engines {
            for sample_index in 1..=count {
                ranked.push((
                    schedule_digest(seed, workload_id, *engine, phase, sample_index),
                    ScheduledTrial {
                        trial_id: format!(
                            "{}-{}-{}-{sample_index:04}",
                            phase.as_str(),
                            workload_id,
                            engine.as_str(),
                        ),
                        sequence: 0,
                        workload_id: (*workload_id).to_owned(),
                        engine: *engine,
                        phase,
                        sample_index,
                    },
                ));
            }
        }
    }
    if randomize_order {
        if phase == TrialPhase::Pilot {
            ranked.sort_by_key(|(digest, trial)| (trial.sample_index, *digest));
        } else {
            ranked.sort_by_key(|entry| entry.0);
        }
    }
    ranked.into_iter().map(|(_, trial)| trial).collect()
}

fn schedule_digest(
    seed: u64,
    workload_id: &str,
    engine: Engine,
    phase: TrialPhase,
    sample_index: u32,
) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(format!(
        "bperf-measurement-order-v{MEASUREMENT_SCHEMA_VERSION}\0"
    ));
    digest.update(seed.to_le_bytes());
    digest.update([0]);
    digest.update(workload_id.as_bytes());
    digest.update([0]);
    digest.update(engine.as_str().as_bytes());
    digest.update([0]);
    digest.update(phase.as_str().as_bytes());
    digest.update([0]);
    digest.update(sample_index.to_le_bytes());
    digest.finalize().into()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn scheduling_is_reproducible_and_covers_each_stratum() {
        let workloads = ["one", "two"];
        let engines = [Engine::Chromium, Engine::Firefox, Engine::Webkit];
        let first = phase_trials(730_241, true, &workloads, &engines, TrialPhase::Final, 7);
        let second = phase_trials(730_241, true, &workloads, &engines, TrialPhase::Final, 7);
        assert_eq!(
            first
                .iter()
                .map(|trial| &trial.trial_id)
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|trial| &trial.trial_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(first.len(), workloads.len() * engines.len() * 7);
        assert_eq!(
            first
                .iter()
                .map(|trial| &trial.trial_id)
                .collect::<HashSet<_>>()
                .len(),
            first.len()
        );
    }

    #[test]
    fn disabling_randomization_preserves_declared_stratum_order() {
        let trials = phase_trials(
            7,
            false,
            &["one", "two"],
            &[Engine::Chromium, Engine::Firefox],
            TrialPhase::Pilot,
            2,
        );
        assert_eq!(trials[0].trial_id, "pilot-one-chromium-0001");
        assert_eq!(trials[1].trial_id, "pilot-one-chromium-0002");
        assert_eq!(trials[2].trial_id, "pilot-one-firefox-0001");
        assert_eq!(trials[4].trial_id, "pilot-two-chromium-0001");
    }

    #[test]
    fn randomization_changes_execution_order_without_changing_ids() {
        let declared = phase_trials(
            7,
            false,
            &["workload"],
            &[Engine::Chromium, Engine::Firefox],
            TrialPhase::Final,
            20,
        );
        let randomized = phase_trials(
            7,
            true,
            &["workload"],
            &[Engine::Chromium, Engine::Firefox],
            TrialPhase::Final,
            20,
        );
        assert_ne!(
            declared
                .iter()
                .map(|trial| &trial.trial_id)
                .collect::<Vec<_>>(),
            randomized
                .iter()
                .map(|trial| &trial.trial_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            declared
                .iter()
                .map(|trial| &trial.trial_id)
                .collect::<HashSet<_>>(),
            randomized
                .iter()
                .map(|trial| &trial.trial_id)
                .collect::<HashSet<_>>()
        );
    }

    #[test]
    fn pilot_randomization_preserves_sample_prefixes() {
        let trials = phase_trials(
            7,
            true,
            &["one", "two"],
            &[Engine::Chromium, Engine::Firefox],
            TrialPhase::Pilot,
            3,
        );

        assert_eq!(
            trials
                .as_chunks::<4>()
                .0
                .iter()
                .map(|round| {
                    round
                        .iter()
                        .map(|trial| trial.sample_index)
                        .collect::<HashSet<_>>()
                })
                .collect::<Vec<_>>(),
            vec![HashSet::from([1]), HashSet::from([2]), HashSet::from([3])]
        );
    }

    #[test]
    fn final_samples_are_complete_trials() {
        let trials = phase_trials(
            7,
            false,
            &["case"],
            &[Engine::Chromium],
            TrialPhase::Final,
            2,
        );

        assert_eq!(
            trials
                .iter()
                .map(|trial| trial.sample_index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(trials[0].trial_id, "final-case-chromium-0001");
        assert_eq!(trials[1].trial_id, "final-case-chromium-0002");
    }
}
