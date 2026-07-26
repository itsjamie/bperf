mod benchmark_runtime;
mod doctor;
mod managed_benchmark;
mod runner;

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use bperf_browser::lab::Engine;
use bperf_decision::{baseline, comparison, lineage};
use bperf_measurement::{sampling, store as measurement};
use bperf_runtime::installation::RuntimeInstallation;
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "bperf",
    version,
    about = "Compare browser benchmark variants without weakening correctness"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Prove that required browser capture capabilities work on this host.
    Doctor(DoctorArgs),
    /// Validate a benchmark and, optionally, a compatible variant.
    Validate(ValidateArgs),
    /// Prepare an immutable measurement set for one variant.
    Plan(PlanArgs),
    /// Run or resume all pending trials for one variant.
    Measure(MeasureArgs),
    /// Measure the current source state and compare it with the promoted baseline.
    Run(RunArgs),
    /// Remeasure a selected candidate as independent promotion evidence.
    Confirm(ConfirmArgs),
    /// Compare a candidate measurement set with a stored or explicit baseline.
    Compare(CompareArgs),
    /// Show the measured optimization history for a benchmark.
    History(HistoryArgs),
    /// Show one measured optimization cycle.
    Show(ShowArgs),
    /// Promote a measured optimization cycle to the current baseline.
    Accept(AcceptArgs),
    /// Manage promoted baseline references.
    Baseline(BaselineArgs),
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum EngineSelection {
    Chromium,
    Firefox,
    Webkit,
    #[default]
    All,
}

impl EngineSelection {
    fn engines(self) -> Vec<Engine> {
        match self {
            Self::Chromium => vec![Engine::Chromium],
            Self::Firefox => vec![Engine::Firefox],
            Self::Webkit => vec![Engine::Webkit],
            Self::All => Engine::ALL.to_vec(),
        }
    }
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Browser engine to prove. `all` enforces the complete bperf contract.
    #[arg(long, value_enum, default_value_t)]
    engine: EngineSelection,

    /// Root directory for immutable doctor-run artifacts.
    #[arg(long, default_value = ".bperf/doctor")]
    artifact_dir: PathBuf,

    /// Override the runtime directory used to locate pinned Playwright browsers.
    #[arg(long)]
    sidecar: Option<PathBuf>,

    /// Emit the complete summary JSON to stdout.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ValidateArgs {
    /// Source benchmark specification.
    benchmark: PathBuf,

    /// Variant descriptor to validate against the benchmark subject.
    #[arg(long)]
    variant: Option<PathBuf>,

    /// Emit the validation summary as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PlanArgs {
    /// Source benchmark specification.
    benchmark: PathBuf,

    /// Variant descriptor to measure.
    variant: PathBuf,

    /// Locked final sample count per workload and engine.
    #[arg(long)]
    final_samples: Option<u32>,

    /// Root directory for immutable measurement sets.
    #[arg(long, default_value = ".bperf/measurements")]
    artifact_dir: PathBuf,

    /// Emit the plan summary as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MeasureArgs {
    /// Source benchmark specification.
    benchmark: PathBuf,

    /// Variant descriptor to measure.
    variant: PathBuf,

    /// Locked final sample count per workload and engine.
    #[arg(long)]
    final_samples: Option<u32>,

    /// Root directory for immutable measurement sets.
    #[arg(long, default_value = ".bperf/measurements")]
    artifact_dir: PathBuf,

    /// Override the runtime directory used to locate pinned Playwright browsers.
    #[arg(long)]
    sidecar: Option<PathBuf>,

    /// Emit the measurement summary as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// TypeScript browser benchmark module.
    benchmark: PathBuf,

    #[command(flatten)]
    execution: ExecutionArgs,

    /// Short hypothesis or description for this measured source change.
    #[arg(long)]
    message: Option<String>,

    /// Emit the measurement and optional comparison result as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfirmArgs {
    /// Optimization cycle to confirm.
    cycle_id: String,

    /// TypeScript browser benchmark module.
    benchmark: PathBuf,

    #[command(flatten)]
    execution: ExecutionArgs,

    /// Emit the confirmation evidence as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExecutionArgs {
    /// Root directory for immutable measurement sets.
    #[arg(long, default_value = ".bperf/measurements")]
    artifact_dir: PathBuf,

    /// Generated internal benchmark state.
    #[arg(long, default_value = ".bperf/managed")]
    state_dir: PathBuf,

    /// Content-addressed fixture bodies.
    #[arg(long, default_value = ".bperf/objects")]
    object_dir: PathBuf,

    /// Directory containing append-only baseline histories.
    #[arg(long, default_value = ".bperf/baselines")]
    registry_dir: PathBuf,

    /// Root directory for comparison reports.
    #[arg(long, default_value = ".bperf/comparisons")]
    comparison_dir: PathBuf,

    /// Root directory for source checkpoints and optimization history.
    #[arg(long, default_value = ".bperf/lineages")]
    lineage_dir: PathBuf,

    /// Approximate measurement-time budget. The minimum evidence floor still applies.
    #[arg(long, default_value = "5m")]
    budget: sampling::RunBudget,

    /// Node.js executable. Defaults to BPERF_NODE, then `node`.
    #[arg(long)]
    node: Option<PathBuf>,

    /// Override the bundled benchmark runtime directory.
    #[arg(long)]
    sidecar: Option<PathBuf>,
}

impl ExecutionArgs {
    fn into_options(self) -> managed_benchmark::ExecutionOptions {
        managed_benchmark::ExecutionOptions {
            artifact_root: self.artifact_dir,
            state_root: self.state_dir,
            object_root: self.object_dir,
            registry_root: self.registry_dir,
            comparison_root: self.comparison_dir,
            lineage_root: self.lineage_dir,
            budget: self.budget,
            node: self.node,
            sidecar: self.sidecar,
        }
    }
}

#[derive(Debug, Args)]
struct CompareArgs {
    /// Candidate measurement-set directory.
    candidate: PathBuf,

    /// Explicit baseline measurement set. Uses the promoted baseline when omitted.
    #[arg(long)]
    baseline: Option<PathBuf>,

    /// Directory containing append-only baseline histories.
    #[arg(long, default_value = ".bperf/baselines")]
    registry_dir: PathBuf,

    /// Root directory for comparison reports.
    #[arg(long, default_value = ".bperf/comparisons")]
    artifact_dir: PathBuf,

    /// Explicit comparison JSON path.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Emit the complete machine-readable verdict JSON to stdout.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    /// Benchmark identifier.
    benchmark_id: String,

    /// Root directory for source checkpoints and optimization history.
    #[arg(long, default_value = ".bperf/lineages")]
    lineage_dir: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    format: HistoryFormatArg,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HistoryFormatArg {
    Text,
    Json,
    AgentContext,
}

impl From<HistoryFormatArg> for lineage::HistoryFormat {
    fn from(format: HistoryFormatArg) -> Self {
        match format {
            HistoryFormatArg::Text => Self::Text,
            HistoryFormatArg::Json => Self::Json,
            HistoryFormatArg::AgentContext => Self::AgentContext,
        }
    }
}

#[derive(Debug, Args)]
struct ShowArgs {
    /// Optimization cycle identifier.
    cycle_id: String,

    /// Root directory for source checkpoints and optimization history.
    #[arg(long, default_value = ".bperf/lineages")]
    lineage_dir: PathBuf,

    /// Include the source delta.
    #[arg(long)]
    diff: bool,

    /// Emit the cycle as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AcceptArgs {
    /// Optimization cycle identifier.
    cycle_id: String,

    /// Root directory for source checkpoints and optimization history.
    #[arg(long, default_value = ".bperf/lineages")]
    lineage_dir: PathBuf,

    /// Directory containing append-only baseline histories.
    #[arg(long, default_value = ".bperf/baselines")]
    registry_dir: PathBuf,

    /// Emit the acceptance event as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BaselineArgs {
    #[command(subcommand)]
    command: BaselineCommand,
}

#[derive(Debug, Subcommand)]
enum BaselineCommand {
    /// Promote a complete measurement set to the current baseline.
    Promote(BaselinePromoteArgs),
    /// Show the current baseline for a benchmark.
    Show(BaselineShowArgs),
}

#[derive(Debug, Args)]
struct BaselinePromoteArgs {
    /// Complete measurement-set directory to promote.
    measurement_set: PathBuf,

    /// Directory containing append-only baseline histories.
    #[arg(long, default_value = ".bperf/baselines")]
    registry_dir: PathBuf,

    /// Emit the baseline reference as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BaselineShowArgs {
    /// Benchmark identifier.
    benchmark_id: String,

    /// Directory containing append-only baseline histories.
    #[arg(long, default_value = ".bperf/baselines")]
    registry_dir: PathBuf,

    /// Emit the baseline reference as JSON.
    #[arg(long)]
    json: bool,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor(args) => {
            let options = doctor::DoctorOptions {
                engines: args.engine.engines(),
                artifact_root: args.artifact_dir,
                runtime: args.sidecar.map_or_else(
                    RuntimeInstallation::discover,
                    RuntimeInstallation::from_root,
                )?,
                json: args.json,
            };
            doctor::run(options)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Validate(args) => {
            measurement::validate(measurement::ValidateOptions {
                benchmark: args.benchmark,
                variant: args.variant,
                json: args.json,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Plan(args) => {
            measurement::plan(measurement::PlanOptions {
                benchmark: args.benchmark,
                variant: args.variant,
                final_samples: args.final_samples,
                artifact_root: args.artifact_dir,
                json: args.json,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Measure(args) => {
            let outcome = runner::run(runner::MeasureOptions {
                benchmark: args.benchmark,
                variant: args.variant,
                sampling: runner::SamplingMode::Fixed(args.final_samples),
                artifact_root: args.artifact_dir,
                runtime: args.sidecar.map_or_else(
                    RuntimeInstallation::discover,
                    RuntimeInstallation::from_root,
                )?,
            })?;
            outcome.report("measure", args.json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(args) => {
            let outcome = managed_benchmark::run(managed_benchmark::RunOptions {
                benchmark: args.benchmark,
                message: args.message,
                execution: args.execution.into_options(),
            })?;
            outcome.report(args.json)?;
            Ok(outcome.exit_code())
        }
        Command::Confirm(args) => {
            let outcome = managed_benchmark::confirm(managed_benchmark::ConfirmOptions {
                benchmark: args.benchmark,
                cycle_id: args.cycle_id,
                execution: args.execution.into_options(),
            })?;
            outcome.report(args.json)?;
            Ok(outcome.exit_code())
        }
        Command::Compare(args) => {
            let outcome = comparison::run(comparison::CompareOptions {
                candidate_root: args.candidate,
                baseline_root: args.baseline,
                registry_root: args.registry_dir,
                artifact_root: args.artifact_dir,
                output: args.output,
            })?;
            outcome.report(args.json)?;
            Ok(outcome.exit_code())
        }
        Command::History(args) => {
            lineage::history(lineage::HistoryOptions {
                benchmark_id: args.benchmark_id,
                root: args.lineage_dir,
                format: args.format.into(),
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Show(args) => {
            lineage::show(lineage::ShowOptions {
                cycle_id: args.cycle_id,
                root: args.lineage_dir,
                diff: args.diff,
                json: args.json,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Accept(args) => {
            let outcome = lineage::accept(lineage::AcceptOptions {
                cycle_id: args.cycle_id,
                root: args.lineage_dir,
                registry_root: args.registry_dir,
            })?;
            outcome.report(args.json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Baseline(args) => match args.command {
            BaselineCommand::Promote(args) => {
                baseline::promote(baseline::PromoteOptions {
                    measurement_root: args.measurement_set,
                    registry_root: args.registry_dir,
                    json: args.json,
                })?;
                Ok(ExitCode::SUCCESS)
            }
            BaselineCommand::Show(args) => {
                baseline::show(baseline::ShowOptions {
                    benchmark_id: args.benchmark_id,
                    registry_root: args.registry_dir,
                    json: args.json,
                })?;
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}
