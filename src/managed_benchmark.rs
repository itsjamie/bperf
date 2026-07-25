//! Materialization of a browser benchmark module for the measurement engine.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    baseline,
    browser_lab::Engine,
    comparison::{self, ComparisonOutcome, ComparisonReport},
    lineage::{self, CycleRecord},
    manifest::{BenchmarkManifest, VariantDescriptor},
    measurement::{self as measurement_store, MeasurementSet},
    runner::{self, MeasureOptions, SamplingMode},
    sampling::RunBudget,
    sidecar_runtime::{SidecarInstallation, node_path as path_value},
};

const STANDARD_TRIAL_POLICY: TrialPolicy = TrialPolicy {
    warmup_samples: 0,
    pilot_samples: 10,
    min_final_samples: 20,
    max_final_samples: 100,
};

#[derive(Clone, Copy)]
struct TrialPolicy {
    warmup_samples: u32,
    pilot_samples: u32,
    min_final_samples: u32,
    max_final_samples: u32,
}

pub struct ExecutionOptions {
    pub artifact_root: PathBuf,
    pub state_root: PathBuf,
    pub object_root: PathBuf,
    pub registry_root: PathBuf,
    pub comparison_root: PathBuf,
    pub lineage_root: PathBuf,
    pub budget: RunBudget,
    pub node: Option<PathBuf>,
    pub sidecar: Option<PathBuf>,
}

pub struct RunOptions {
    pub benchmark: PathBuf,
    pub message: Option<String>,
    pub execution: ExecutionOptions,
}

pub(crate) fn run(options: RunOptions) -> Result<RunOutcome> {
    let execution = options.execution;
    let node = execution
        .node
        .clone()
        .or_else(|| std::env::var_os("BPERF_NODE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("node"));
    let inputs = materialize(
        &options.benchmark,
        &execution.state_root,
        &execution.object_root,
        &node,
        STANDARD_TRIAL_POLICY,
    )?;
    let measurement = runner::run(MeasureOptions {
        benchmark: inputs.benchmark,
        variant: inputs.variant,
        sampling: SamplingMode::Adaptive {
            budget: execution.budget,
            cohort: None,
        },
        artifact_root: execution.artifact_root,
        node: Some(node),
        sidecar: execution.sidecar,
    })?;
    let comparison = compare_current(
        &measurement,
        execution.registry_root,
        execution.comparison_root,
    )?;
    let cycle = lineage::record_run(lineage::RecordRunOptions {
        root: execution.lineage_root,
        workspace_root: inputs.workspace_root,
        source_files: inputs.source_files,
        measurement_root: measurement.measurement_root().to_owned(),
        message: options.message,
        comparison: comparison.as_ref().map(ComparisonOutcome::summary),
    })?;
    let exit_code = comparison
        .as_ref()
        .map_or(0, ComparisonOutcome::exit_code_value);
    let index_path = record_measurement_index(MeasurementIndexOptions {
        measurement_root: measurement.measurement_root(),
        benchmark_id: measurement.benchmark_id(),
        measurement_set_id: measurement.measurement_set_id(),
        recorded_at_unix_ms: cycle.recorded_at_unix_ms(),
        exit_code,
        outcome: cycle.outcome(),
        event: "cycle",
        record_id: cycle.cycle_id(),
        comparison_path: comparison.as_ref().map(ComparisonOutcome::report_path),
    })?;
    Ok(RunOutcome {
        measurement,
        comparison,
        cycle,
        index_path,
    })
}

pub struct ConfirmOptions {
    pub benchmark: PathBuf,
    pub cycle_id: String,
    pub execution: ExecutionOptions,
}

pub(crate) fn confirm(options: ConfirmOptions) -> Result<ConfirmOutcome> {
    confirm_with_policy(options, STANDARD_TRIAL_POLICY)
}

fn confirm_with_policy(
    options: ConfirmOptions,
    trial_policy: TrialPolicy,
) -> Result<ConfirmOutcome> {
    let execution = options.execution;
    let target = lineage::confirmation_target(&execution.lineage_root, &options.cycle_id)?;
    let node = execution
        .node
        .clone()
        .or_else(|| std::env::var_os("BPERF_NODE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("node"));
    let inputs = materialize(
        &options.benchmark,
        &execution.state_root,
        &execution.object_root,
        &node,
        trial_policy,
    )?;
    let original = MeasurementSet::open(target.candidate_measurement_path())?;
    let current_benchmark = BenchmarkManifest::load(&inputs.benchmark)?;
    let current_variant = VariantDescriptor::load(&inputs.variant)?;
    if current_benchmark.benchmark_id() != target.benchmark_id()
        || current_benchmark.source_sha256() != original.benchmark_sha256()
        || current_variant.source_sha256() != original.variant_sha256()
    {
        bail!(
            "current benchmark source does not match cycle {}",
            target.cycle_id()
        );
    }

    let baseline_root = baseline::current_path(&execution.registry_root, target.benchmark_id())?;
    let current_baseline = MeasurementSet::open(&baseline_root)?;
    if current_baseline.measurement_set_id() != target.baseline_measurement_set() {
        bail!(
            "cycle {} was compared with baseline {}, but the current baseline is {}",
            target.cycle_id(),
            target.baseline_measurement_set(),
            current_baseline.measurement_set_id()
        );
    }

    let workspace_root = inputs.workspace_root;
    let source_files = inputs.source_files;
    let measurement = runner::run(MeasureOptions {
        benchmark: inputs.benchmark,
        variant: inputs.variant,
        sampling: SamplingMode::Adaptive {
            budget: execution.budget,
            cohort: Some(format!("confirmation:{}", target.cycle_id())),
        },
        artifact_root: execution.artifact_root,
        node: Some(node),
        sidecar: execution.sidecar,
    })?;
    let comparison = comparison::run(comparison::CompareOptions {
        candidate_root: measurement.measurement_root().to_owned(),
        baseline_root: Some(baseline_root),
        registry_root: execution.registry_root,
        artifact_root: execution.comparison_root,
        output: None,
    })?;
    let confirmation = lineage::record_confirmation(lineage::RecordConfirmationOptions {
        root: execution.lineage_root,
        cycle_id: options.cycle_id,
        workspace_root,
        source_files,
        measurement_root: measurement.measurement_root().to_owned(),
        comparison: comparison.summary(),
    })?;
    let index_path = record_measurement_index(MeasurementIndexOptions {
        measurement_root: measurement.measurement_root(),
        benchmark_id: measurement.benchmark_id(),
        measurement_set_id: measurement.measurement_set_id(),
        recorded_at_unix_ms: confirmation.recorded_at_unix_ms(),
        exit_code: comparison.exit_code_value(),
        outcome: confirmation.outcome(),
        event: "confirmation",
        record_id: confirmation.confirmation_id(),
        comparison_path: Some(comparison.report_path()),
    })?;
    Ok(ConfirmOutcome {
        measurement,
        comparison,
        confirmation,
        index_path,
    })
}

pub(crate) struct ConfirmOutcome {
    measurement: runner::MeasurementOutcome,
    comparison: ComparisonOutcome,
    confirmation: lineage::ConfirmationRecord,
    index_path: PathBuf,
}

impl ConfirmOutcome {
    pub(crate) fn report(&self, json: bool) -> Result<()> {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&ConfirmationReport {
                    schema_version: 1,
                    status: self.confirmation.outcome(),
                    measurement: &self.measurement,
                    comparison: self.comparison.report_data(),
                    confirmation: &self.confirmation,
                    measurement_index: &self.index_path,
                })?
            );
        } else {
            println!("bperf confirm: {}", self.confirmation.outcome());
            self.measurement.report_details();
            self.comparison.report_details();
            println!("  confirmation: {}", self.confirmation.confirmation_id());
            println!("  measurement index: {}", self.index_path.display());
        }
        Ok(())
    }

    pub(crate) fn exit_code(&self) -> std::process::ExitCode {
        self.comparison.exit_code()
    }
}

#[derive(Serialize)]
struct ConfirmationReport<'a> {
    schema_version: u32,
    status: &'a str,
    measurement: &'a runner::MeasurementOutcome,
    comparison: &'a ComparisonReport,
    confirmation: &'a lineage::ConfirmationRecord,
    measurement_index: &'a Path,
}

fn compare_current(
    measurement: &runner::MeasurementOutcome,
    registry_root: PathBuf,
    comparison_root: PathBuf,
) -> Result<Option<ComparisonOutcome>> {
    let comparison =
        match baseline::current_path_if_present(&registry_root, measurement.benchmark_id())? {
            Some(baseline_root) => Some(comparison::run(comparison::CompareOptions {
                candidate_root: measurement.measurement_root().to_owned(),
                baseline_root: Some(baseline_root),
                registry_root,
                artifact_root: comparison_root,
                output: None,
            })?),
            None => None,
        };
    Ok(comparison)
}

pub(crate) struct RunOutcome {
    measurement: runner::MeasurementOutcome,
    comparison: Option<ComparisonOutcome>,
    cycle: CycleRecord,
    index_path: PathBuf,
}

impl RunOutcome {
    pub(crate) fn report(&self, json: bool) -> Result<()> {
        let status = self.cycle.outcome();
        if json {
            let report = RunReport {
                schema_version: 1,
                status,
                measurement: &self.measurement,
                comparison: self.comparison.as_ref().map(ComparisonOutcome::report_data),
                cycle: &self.cycle,
                measurement_index: &self.index_path,
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("bperf run: {status}");
            self.measurement.report_details();
            if let Some(comparison) = &self.comparison {
                comparison.report_details();
            } else {
                self.measurement.report_engine_results()?;
                println!("  comparison: no promoted baseline");
            }
            println!("  cycle: {}", self.cycle.cycle_id());
            println!(
                "  source change: bperf show {} --diff",
                self.cycle.cycle_id()
            );
            println!("  measurement index: {}", self.index_path.display());
        }
        Ok(())
    }

    pub(crate) fn exit_code(&self) -> std::process::ExitCode {
        self.comparison.as_ref().map_or(
            std::process::ExitCode::SUCCESS,
            ComparisonOutcome::exit_code,
        )
    }
}

#[derive(Serialize)]
struct RunReport<'a> {
    schema_version: u32,
    status: &'a str,
    measurement: &'a runner::MeasurementOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    comparison: Option<&'a ComparisonReport>,
    cycle: &'a CycleRecord,
    measurement_index: &'a Path,
}

#[derive(Clone, Copy)]
struct MeasurementIndexOptions<'a> {
    measurement_root: &'a Path,
    benchmark_id: &'a str,
    measurement_set_id: &'a str,
    recorded_at_unix_ms: u64,
    exit_code: u8,
    outcome: &'a str,
    event: &'static str,
    record_id: &'a str,
    comparison_path: Option<&'a Path>,
}

#[derive(Serialize)]
struct MeasurementIndexRecord<'a> {
    schema_version: u32,
    recorded_at_unix_ms: u64,
    exit_code: u8,
    outcome: &'a str,
    event: &'static str,
    record_id: &'a str,
    benchmark_id: &'a str,
    measurement_set_id: &'a str,
    measurement_path: &'a Path,
    measurement_summary: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    comparison_path: Option<&'a Path>,
}

fn record_measurement_index(options: MeasurementIndexOptions<'_>) -> Result<PathBuf> {
    if options.recorded_at_unix_ms == 0 {
        bail!("measurement index has no creation timestamp");
    }
    if options.exit_code > 2 {
        bail!(
            "measurement index has unsupported exit code {}",
            options.exit_code
        );
    }
    let measurement_summary = options.measurement_root.join("summary.json");
    if !measurement_summary.is_file() {
        bail!(
            "measurement index summary does not exist: {}",
            measurement_summary.display()
        );
    }
    let artifact_root = options
        .measurement_root
        .parent()
        .context("measurement set has no artifact root")?;
    let index_root = artifact_root.join("index");
    fs::create_dir_all(&index_root)
        .with_context(|| format!("failed to create {}", index_root.display()))?;
    let index_path = index_root.join(format!(
        "{:013}-exit-{}-{}.json",
        options.recorded_at_unix_ms, options.exit_code, options.record_id
    ));
    let record = MeasurementIndexRecord {
        schema_version: 1,
        recorded_at_unix_ms: options.recorded_at_unix_ms,
        exit_code: options.exit_code,
        outcome: options.outcome,
        event: options.event,
        record_id: options.record_id,
        benchmark_id: options.benchmark_id,
        measurement_set_id: options.measurement_set_id,
        measurement_path: options.measurement_root,
        measurement_summary,
        comparison_path: options.comparison_path,
    };
    measurement_store::write_immutable(
        &index_path,
        format!("{}\n", serde_json::to_string_pretty(&record)?).as_bytes(),
    )?;
    Ok(index_path)
}

struct MaterializedInputs {
    benchmark: PathBuf,
    variant: PathBuf,
    workspace_root: PathBuf,
    source_files: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedDescription {
    schema_version: u32,
    benchmark_id: String,
    cases: Vec<ManagedCase>,
    source_files: Vec<PathBuf>,
    fixture_files: Vec<PathBuf>,
    fixture_lock: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedCase {
    id: String,
    expectation: ManagedExpectation,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ManagedExpectation {
    Exact { value: Value },
}

impl ManagedExpectation {
    fn expected(&self) -> &Value {
        match self {
            Self::Exact { value } => value,
        }
    }
}

fn materialize(
    benchmark: &Path,
    state_root: &Path,
    object_root: &Path,
    node: &Path,
    trial_policy: TrialPolicy,
) -> Result<MaterializedInputs> {
    let root = fs::canonicalize(std::env::current_dir()?)
        .context("failed to resolve the benchmark workspace")?;
    let benchmark = fs::canonicalize(benchmark)
        .with_context(|| format!("failed to resolve benchmark {}", benchmark.display()))?;
    if !benchmark.starts_with(&root) {
        bail!(
            "benchmark {} is outside the current workspace {}",
            benchmark.display(),
            root.display()
        );
    }

    let state_root = absolute_directory(state_root)?;
    let object_root = absolute_directory(object_root)?;
    let key = benchmark_key(&benchmark);
    let generated_root = state_root.join(key);
    fs::create_dir_all(&generated_root)
        .with_context(|| format!("failed to create {}", generated_root.display()))?;
    let generated_root = fs::canonicalize(&generated_root)?;
    let fixture_lock = generated_root.join("fixture-lock.json");
    let sidecar = SidecarInstallation::discover()?;

    let description = describe(
        &root,
        &benchmark,
        &fixture_lock,
        &object_root,
        node,
        &sidecar,
    )?;
    validate_description(&description)?;
    write_inputs(
        &root,
        &benchmark,
        &generated_root,
        node,
        &sidecar,
        &description,
        trial_policy,
    )
}

fn absolute_directory(path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))?;
    fs::canonicalize(path).with_context(|| format!("failed to resolve {}", path.display()))
}

fn benchmark_key(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bperf-managed-benchmark-path-v1\0");
    digest.update(path.to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())[..16].to_owned()
}

fn describe(
    root: &Path,
    benchmark: &Path,
    fixture_lock: &Path,
    object_root: &Path,
    node: &Path,
    sidecar: &SidecarInstallation,
) -> Result<ManagedDescription> {
    let host = sidecar.benchmark_host();
    let output = Command::new(node)
        .arg("--disable-warning=ExperimentalWarning")
        .arg(path_value(&host))
        .arg("describe")
        .arg(path_value(benchmark))
        .arg("--root")
        .arg(path_value(root))
        .arg("--lock")
        .arg(path_value(fixture_lock))
        .arg("--cache")
        .arg(path_value(object_root))
        .output()
        .with_context(|| {
            format!(
                "failed to describe benchmark with Node executable {}",
                node.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "benchmark description failed with {}{}",
            output.status,
            if output.stderr.is_empty() {
                String::new()
            } else {
                format!(": {}", String::from_utf8_lossy(&output.stderr))
            }
        );
    }
    serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "benchmark host {} emitted an invalid description",
            host.display()
        )
    })
}

fn validate_description(description: &ManagedDescription) -> Result<()> {
    if description.schema_version != 1 {
        bail!(
            "unsupported managed benchmark schema {}; expected 1",
            description.schema_version
        );
    }
    if description.cases.is_empty() {
        bail!("managed benchmark contains no cases");
    }
    if description.source_files.is_empty() {
        bail!("managed benchmark resolved no source files");
    }
    if !description.fixture_lock.is_file() {
        bail!(
            "managed benchmark fixture lock does not exist: {}",
            description.fixture_lock.display()
        );
    }
    for file in description
        .source_files
        .iter()
        .chain(&description.fixture_files)
    {
        if !file.is_file() {
            bail!(
                "managed benchmark identity file is missing: {}",
                file.display()
            );
        }
    }
    Ok(())
}

fn write_inputs(
    root: &Path,
    benchmark_module: &Path,
    generated_root: &Path,
    node: &Path,
    sidecar: &SidecarInstallation,
    description: &ManagedDescription,
    trial_policy: TrialPolicy,
) -> Result<MaterializedInputs> {
    let benchmark_host = sidecar.benchmark_host();
    let workload_root = generated_root.join("workloads");
    fs::create_dir_all(&workload_root)?;

    let mut benchmark_identity_files = BTreeSet::from([
        path_value(benchmark_module),
        path_value(&description.fixture_lock),
    ]);
    benchmark_identity_files.extend(
        description
            .fixture_files
            .iter()
            .map(|file| path_value(file)),
    );
    let identity_files: Vec<_> = benchmark_identity_files.into_iter().collect();

    let mut workloads = Vec::new();
    for case in &description.cases {
        let trace_file = workload_root.join(format!("{}.jsonl", case.id));
        write_generated(
            &trace_file,
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "case_id": case.id,
                    "expected": case.expectation.expected(),
                }))?
            )
            .as_bytes(),
        )?;
        workloads.push(json!({
            "id": case.id,
            "trace_file": path_value(&trace_file),
            "identity_files": identity_files,
            "verifier": {
                "builtin": "exact",
            },
        }));
    }

    let benchmark = json!({
        "schema_version": 1,
        "benchmark": {
            "id": description.benchmark_id,
            "subject": description.benchmark_id,
        },
        "workloads": workloads,
        "browser": {
            "engines": Engine::ALL,
            "mode": "headless",
            "viewport": { "width": 1440, "height": 900 },
            "locale": "en-US",
            "timezone": "UTC",
            "color_scheme": "light",
            "cache": "cold",
            "network": { "policy": "local-only" },
            "fresh_profile_per_trial": true,
        },
        "captures": {
            "cpu_profile": true,
            "js_heap": true,
        },
        "trials": {
            "mode": "auto",
            "randomize_order": true,
            "schedule_seed": 730241,
            "warmup_samples": trial_policy.warmup_samples,
            "pilot_samples": trial_policy.pilot_samples,
            "min_final_samples": trial_policy.min_final_samples,
            "max_final_samples": trial_policy.max_final_samples,
        },
        "statistics": {
            "confidence": 0.95,
            "bootstrap_samples": 10000,
            "primary_metrics": [
                "workload.wall_ms",
                "browser.cpu_profile.active_ms",
                "browser.js_heap.live_bytes",
            ],
            "minimum_effect_pct": {
                "workload.wall_ms": 5.0,
                "browser.cpu_profile.active_ms": 5.0,
                "browser.js_heap.live_bytes": 5.0,
            },
            "correctness": {
                "minimum_success_rate": 0.95,
                "max_regression_percentage_points": 1.0,
            },
            "cross_engine_policy": "strict_all",
            "protected_metric_max_regression_pct": 3.0,
        },
    });
    let benchmark_path = generated_root.join("benchmark.json");
    write_json(&benchmark_path, &benchmark)?;

    let mut implementation_files = BTreeSet::from([path_value(benchmark_module)]);
    implementation_files.extend(
        sidecar
            .identity_files()?
            .iter()
            .map(|file| path_value(file)),
    );
    implementation_files.extend(description.source_files.iter().map(|file| path_value(file)));
    let variant = json!({
        "schema_version": 1,
        "id": "worktree",
        "subject": description.benchmark_id,
        "implementation": {
            "files": implementation_files
                .into_iter()
                .collect::<Vec<_>>(),
        },
        "adapter": {
            "command": [
                path_value(node),
                "--disable-warning=ExperimentalWarning",
                path_value(&benchmark_host),
                "serve",
                path_value(benchmark_module),
                "--root",
                path_value(root),
                "--lock",
                path_value(&description.fixture_lock),
            ],
            "ready": {
                "protocol": "stdio-json",
                "timeout_seconds": 15,
            },
        },
    });
    let variant_path = generated_root.join("variant.json");
    write_json(&variant_path, &variant)?;

    Ok(MaterializedInputs {
        benchmark: benchmark_path,
        variant: variant_path,
        workspace_root: root.to_owned(),
        source_files: description.source_files.clone(),
    })
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_generated(
        path,
        format!("{}\n", serde_json::to_string_pretty(value)?).as_bytes(),
    )
}

fn write_generated(path: &Path, content: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tempfile::tempdir;

    use super::*;
    use crate::measurement::MeasurementSet;

    #[cfg(windows)]
    #[test]
    fn paths_crossing_into_node_do_not_use_windows_verbatim_syntax() {
        assert_eq!(
            path_value(Path::new(r"\\?\C:\workspace\benchmark.ts")),
            r"C:\workspace\benchmark.ts"
        );
        assert_eq!(
            path_value(Path::new(r"\\?\UNC\server\share\benchmark.ts")),
            r"\\server\share\benchmark.ts"
        );
    }

    #[test]
    fn measurement_index_names_sort_by_creation_and_expose_exit_codes() {
        let temporary = tempdir().unwrap();
        let measurement_root = temporary
            .path()
            .join("measurements")
            .join("measure-v4-test");
        fs::create_dir_all(&measurement_root).unwrap();
        fs::write(measurement_root.join("summary.json"), b"{}\n").unwrap();

        let later = record_measurement_index(MeasurementIndexOptions {
            measurement_root: &measurement_root,
            benchmark_id: "parser",
            measurement_set_id: "measure-v4-test",
            recorded_at_unix_ms: 10,
            exit_code: 1,
            outcome: "negative",
            event: "cycle",
            record_id: "cycle-later",
            comparison_path: Some(Path::new("comparisons/later/comparison.json")),
        })
        .unwrap();
        let earlier_options = MeasurementIndexOptions {
            measurement_root: &measurement_root,
            benchmark_id: "parser",
            measurement_set_id: "measure-v4-test",
            recorded_at_unix_ms: 9,
            exit_code: 0,
            outcome: "positive",
            event: "cycle",
            record_id: "cycle-earlier",
            comparison_path: Some(Path::new("comparisons/earlier/comparison.json")),
        };
        let earlier = record_measurement_index(earlier_options).unwrap();
        assert_eq!(
            record_measurement_index(earlier_options).unwrap(),
            earlier,
            "an exact retry must reuse its index receipt"
        );

        let mut names = fs::read_dir(earlier.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "0000000000009-exit-0-cycle-earlier.json",
                "0000000000010-exit-1-cycle-later.json",
            ]
        );

        let receipt: Value =
            serde_json::from_slice(&fs::read(later).unwrap()).expect("valid index receipt");
        assert_eq!(receipt["recorded_at_unix_ms"], 10);
        assert_eq!(receipt["exit_code"], 1);
        assert_eq!(receipt["outcome"], "negative");
        assert_eq!(receipt["measurement_set_id"], "measure-v4-test");
        assert!(
            receipt["measurement_summary"]
                .as_str()
                .unwrap()
                .ends_with("summary.json")
        );
        assert!(
            receipt["comparison_path"]
                .as_str()
                .unwrap()
                .ends_with("comparison.json")
        );
    }

    #[test]
    #[ignore = "requires Node and all three Playwright browsers"]
    fn managed_benchmark_satisfies_every_engine_contract() {
        let node = PathBuf::from(
            std::env::var_os("BPERF_NODE")
                .expect("set BPERF_NODE to the Node executable used by the sidecar"),
        );
        let temporary = tempdir().unwrap();
        let benchmark = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples")
            .join("managed")
            .join("fragment-parser.bench.ts");
        let inputs = materialize(
            &benchmark,
            &temporary.path().join("managed"),
            &temporary.path().join("objects"),
            &node,
            TrialPolicy {
                warmup_samples: 0,
                pilot_samples: 2,
                min_final_samples: 1,
                max_final_samples: 2,
            },
        )
        .unwrap();
        let variant = fs::read_to_string(&inputs.variant).unwrap();
        assert!(
            variant.contains("fragment-checksum.ts"),
            "modules loaded by setup or measurement must participate in variant identity"
        );
        let workspace_root = inputs.workspace_root.clone();
        let source_files = inputs.source_files.clone();
        let measurement_root = temporary.path().join("measurements");
        let outcome = runner::run(MeasureOptions {
            benchmark: inputs.benchmark,
            variant: inputs.variant,
            sampling: SamplingMode::Adaptive {
                budget: "2m".parse().unwrap(),
                cohort: None,
            },
            artifact_root: measurement_root.clone(),
            node: Some(node.clone()),
            sidecar: None,
        })
        .unwrap();

        let root = fs::read_dir(&measurement_root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(!root.join("preflight").exists());
        assert!(!root.join("workloads").exists());
        let measurement = MeasurementSet::open(&root).unwrap();
        assert!(measurement.final_is_complete());
        let sampling = measurement.sampling_decision().unwrap();
        assert_eq!(sampling.strata.len(), 3);
        assert!(
            sampling
                .strata
                .iter()
                .all(|stratum| stratum.batch_size >= 1)
        );
        assert!(sampling.strata.iter().any(|stratum| stratum.batch_size > 1));
        assert!((3..=6).contains(&sampling.selected_final_trials));
        assert_eq!(
            measurement
                .schedule
                .trials
                .iter()
                .map(|trial| trial.engine)
                .collect::<HashSet<_>>(),
            HashSet::from(Engine::ALL)
        );
        let records: Vec<Value> = fs::read_to_string(root.join("trials.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 6 + sampling.selected_final_trials as usize);
        assert!(records.iter().all(|record| {
            record["valid"] == true
                && record["success"] == true
                && record["artifacts"]
                    .as_array()
                    .is_some_and(|items| items.len() == 3)
        }));
        let retention: Value =
            serde_json::from_slice(&fs::read(root.join("artifact-retention.json")).unwrap())
                .unwrap();
        let retained_paths: HashSet<_> = retention["selections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|selection| selection["artifact"]["path"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(retained_paths.len(), 9);
        assert_eq!(
            retention["summary"]["discarded_artifacts"],
            records
                .iter()
                .map(|record| record["artifacts"].as_array().unwrap().len())
                .sum::<usize>()
                - retained_paths.len()
        );
        for artifact in records
            .iter()
            .flat_map(|record| record["artifacts"].as_array().unwrap())
        {
            let path = artifact["path"].as_str().unwrap();
            assert_eq!(
                root.join(path).is_file(),
                retained_paths.contains(path),
                "artifact retention mismatch for {path}"
            );
        }

        let registry_root = temporary.path().join("baselines");
        baseline::promote(baseline::PromoteOptions {
            measurement_root: outcome.measurement_root().to_owned(),
            registry_root: registry_root.clone(),
            json: false,
        })
        .unwrap();
        let comparison_root = temporary.path().join("comparisons");
        let comparison = compare_current(&outcome, registry_root.clone(), comparison_root.clone())
            .unwrap()
            .expect("a promoted baseline must produce a comparison");
        let comparison_summary = comparison.summary();
        let expected_outcome = comparison_summary.verdict.clone();
        let cycle = lineage::record_run(lineage::RecordRunOptions {
            root: temporary.path().join("lineages"),
            workspace_root,
            source_files,
            measurement_root: outcome.measurement_root().to_owned(),
            message: Some("verify managed lineage".to_owned()),
            comparison: Some(comparison_summary),
        })
        .unwrap();
        assert_eq!(cycle.outcome(), expected_outcome);
        assert!(
            fs::read_dir(comparison_root).unwrap().any(|entry| entry
                .unwrap()
                .path()
                .join("comparison.json")
                .is_file())
        );

        let confirmation = confirm_with_policy(
            ConfirmOptions {
                benchmark,
                cycle_id: cycle.cycle_id().to_owned(),
                execution: ExecutionOptions {
                    artifact_root: measurement_root,
                    state_root: temporary.path().join("managed"),
                    object_root: temporary.path().join("objects"),
                    registry_root,
                    comparison_root: temporary.path().join("comparisons"),
                    lineage_root: temporary.path().join("lineages"),
                    budget: "2m".parse().unwrap(),
                    node: Some(node),
                    sidecar: None,
                },
            },
            TrialPolicy {
                warmup_samples: 0,
                pilot_samples: 2,
                min_final_samples: 1,
                max_final_samples: 2,
            },
        )
        .unwrap();
        assert!(
            confirmation
                .confirmation
                .confirmation_id()
                .starts_with("confirmation-")
        );
        let confirmation_summary = confirmation.comparison.summary();
        assert!(
            confirmation_summary.engines.iter().all(|engine| engine
                .anchor
                .as_ref()
                .is_some_and(|anchor| anchor.status == "stable")),
            "confirmation anchors were not stable: {:#?}",
            confirmation_summary.engines
        );
    }
}
