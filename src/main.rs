mod benchmark_host;
mod benchmark_runtime;
mod doctor;
mod fixtures;
mod history_tui;
mod managed_benchmark;
mod project_modules;
mod run_tui;
mod runner;
mod terminal_ui;

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::{Result, bail};
use bperf_browser::lab::Engine;
use bperf_decision::{baseline, comparison, lineage};
use bperf_measurement::{sampling, store as measurement};
use bperf_runtime::installation::{BrowserInstallation, BrowserName};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "bperf",
    version,
    about = "Measure browser code and decide whether a change helped",
    arg_required_else_help = true,
    after_help = "Quick start:
  bperf browsers install
  bperf doctor
  bperf run
  bperf run benchmarks/parser.bench.ts -m \"Establish parser baseline\"
  bperf accept

Inspect the latest work with `bperf show --diff` or `bperf history`.
Advanced integration commands: validate, plan, measure, compare, baseline."
)]
struct Cli {
    /// Directory for bperf measurements, history, baselines, and generated state.
    #[arg(long, global = true, default_value = ".bperf", value_name = "DIR")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(name = "__benchmark-host", hide = true)]
    BenchmarkHost(BenchmarkHostArgs),
    /// Measure the current source state and compare it with the promoted baseline.
    Run(RunArgs),
    /// Show one measured optimization cycle.
    Show(ShowArgs),
    /// Promote a measured optimization cycle to the current baseline.
    Accept(AcceptArgs),
    /// Explore measured cycles, evidence, and promotion readiness.
    History(HistoryArgs),
    /// Remeasure a selected candidate as independent promotion evidence.
    Confirm(ConfirmArgs),
    /// Prove that required browser capture capabilities work on this host.
    Doctor(DoctorArgs),
    /// Install the pinned browser builds used by bperf.
    Browsers(BrowsersArgs),
    /// Validate a benchmark and, optionally, a compatible variant.
    #[command(hide = true)]
    Validate(ValidateArgs),
    /// Prepare an immutable measurement set for one variant.
    #[command(hide = true)]
    Plan(PlanArgs),
    /// Run or resume all pending trials for one variant.
    #[command(hide = true)]
    Measure(MeasureArgs),
    /// Compare a candidate measurement set with a stored or explicit baseline.
    #[command(hide = true)]
    Compare(CompareArgs),
    /// Manage promoted baseline references.
    #[command(hide = true)]
    Baseline(BaselineArgs),
}

#[derive(Debug, Args)]
struct BenchmarkHostArgs {
    #[arg(long)]
    root: PathBuf,

    #[arg(long)]
    benchmark: PathBuf,

    #[arg(long)]
    fixture_lock: PathBuf,

    #[arg(long)]
    bundle: PathBuf,

    #[arg(long)]
    bundle_metadata: PathBuf,
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

    fn browsers(self) -> Vec<BrowserName> {
        match self {
            Self::Chromium => vec![BrowserName::ChromiumHeadlessShell],
            Self::Firefox => vec![BrowserName::Firefox],
            Self::Webkit => vec![BrowserName::Webkit],
            Self::All => vec![
                BrowserName::ChromiumHeadlessShell,
                BrowserName::Firefox,
                BrowserName::Webkit,
            ],
        }
    }
}

#[derive(Debug, Args)]
struct BrowsersArgs {
    #[command(subcommand)]
    command: BrowsersCommand,
}

#[derive(Debug, Subcommand)]
enum BrowsersCommand {
    /// Download the browser builds pinned by this bperf version.
    Install(BrowserInstallArgs),
}

#[derive(Debug, Args)]
struct BrowserInstallArgs {
    /// Browser engine to install.
    #[arg(short, long, value_enum, default_value_t)]
    engine: EngineSelection,

    /// Also install operating-system dependencies required by the browsers.
    #[arg(long)]
    with_deps: bool,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Browser engine to prove. `all` enforces the complete bperf contract.
    #[arg(short, long, value_enum, default_value_t)]
    engine: EngineSelection,

    #[arg(long, hide = true)]
    artifact_dir: Option<PathBuf>,

    /// Emit the complete summary JSON to stdout.
    #[arg(short, long)]
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
    #[arg(short, long)]
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

    #[arg(long, hide = true)]
    artifact_dir: Option<PathBuf>,

    /// Emit the plan summary as JSON.
    #[arg(short, long)]
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

    #[arg(long, hide = true)]
    artifact_dir: Option<PathBuf>,

    /// Emit the measurement summary as JSON.
    #[arg(short, long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// TypeScript benchmark module or directory. Omit it to browse ./benchmarks.
    benchmark: Option<PathBuf>,

    #[command(flatten)]
    execution: ExecutionArgs,

    /// Short hypothesis or description for this measured source change.
    #[arg(short, long)]
    message: Option<String>,

    /// Emit the measurement and optional comparison result as JSON.
    #[arg(short, long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConfirmArgs {
    /// TypeScript browser benchmark module.
    benchmark: PathBuf,

    /// Cycle ID, unique ID prefix, or `latest`.
    #[arg(default_value = "latest")]
    cycle_id: String,

    #[command(flatten)]
    execution: ExecutionArgs,

    /// Emit the confirmation evidence as JSON.
    #[arg(short, long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExecutionArgs {
    #[arg(long, hide = true)]
    artifact_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    state_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    object_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    registry_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    comparison_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    lineage_dir: Option<PathBuf>,

    /// Approximate measurement-time budget. The minimum evidence floor still applies.
    #[arg(short, long, default_value = "5m")]
    budget: sampling::RunBudget,
}

impl ExecutionArgs {
    fn into_options(self, data: &DataDirectory) -> managed_benchmark::ExecutionOptions {
        managed_benchmark::ExecutionOptions {
            artifact_root: data.measurements(self.artifact_dir),
            state_root: data.managed(self.state_dir),
            object_root: data.objects(self.object_dir),
            registry_root: data.baselines(self.registry_dir),
            comparison_root: data.comparisons(self.comparison_dir),
            lineage_root: data.lineages(self.lineage_dir),
            budget: self.budget,
        }
    }
}

#[derive(Debug)]
struct DataDirectory {
    root: PathBuf,
}

impl DataDirectory {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn doctor(&self, override_path: Option<PathBuf>) -> PathBuf {
        self.resolve(override_path, "doctor")
    }

    fn measurements(&self, override_path: Option<PathBuf>) -> PathBuf {
        self.resolve(override_path, "measurements")
    }

    fn managed(&self, override_path: Option<PathBuf>) -> PathBuf {
        self.resolve(override_path, "managed")
    }

    fn objects(&self, override_path: Option<PathBuf>) -> PathBuf {
        self.resolve(override_path, "objects")
    }

    fn baselines(&self, override_path: Option<PathBuf>) -> PathBuf {
        self.resolve(override_path, "baselines")
    }

    fn comparisons(&self, override_path: Option<PathBuf>) -> PathBuf {
        self.resolve(override_path, "comparisons")
    }

    fn lineages(&self, override_path: Option<PathBuf>) -> PathBuf {
        self.resolve(override_path, "lineages")
    }

    fn resolve(&self, override_path: Option<PathBuf>, child: &str) -> PathBuf {
        override_path.unwrap_or_else(|| self.root.join(child))
    }
}

#[derive(Debug, Args)]
struct CompareArgs {
    /// Candidate measurement-set directory.
    candidate: PathBuf,

    /// Explicit baseline measurement set. Uses the promoted baseline when omitted.
    #[arg(long)]
    baseline: Option<PathBuf>,

    #[arg(long, hide = true)]
    registry_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    artifact_dir: Option<PathBuf>,

    /// Explicit comparison JSON path.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Emit the complete machine-readable verdict JSON to stdout.
    #[arg(short, long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    /// Benchmark identifier. Omit it to use the latest measured benchmark.
    benchmark_id: Option<String>,

    #[arg(long, hide = true)]
    lineage_dir: Option<PathBuf>,

    /// Non-interactive output format. Supplying this keeps history on stdout.
    #[arg(short, long, value_enum)]
    format: Option<HistoryFormatArg>,
}

impl HistoryArgs {
    fn is_bare(&self) -> bool {
        self.benchmark_id.is_none() && self.lineage_dir.is_none() && self.format.is_none()
    }
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
    /// Cycle ID, unique ID prefix, or `latest`.
    #[arg(default_value = "latest")]
    cycle_id: String,

    /// Restrict cycle selection to one benchmark stream.
    #[arg(long, value_name = "ID")]
    benchmark: Option<String>,

    #[arg(long, hide = true)]
    lineage_dir: Option<PathBuf>,

    /// Include the source delta.
    #[arg(short, long)]
    diff: bool,

    /// Emit the cycle as JSON.
    #[arg(short, long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AcceptArgs {
    /// Cycle ID, unique ID prefix, or `latest`.
    #[arg(default_value = "latest")]
    cycle_id: String,

    /// Restrict cycle selection to one benchmark stream.
    #[arg(long, value_name = "ID")]
    benchmark: Option<String>,

    #[arg(long, hide = true)]
    lineage_dir: Option<PathBuf>,

    #[arg(long, hide = true)]
    registry_dir: Option<PathBuf>,

    /// Emit the acceptance event as JSON.
    #[arg(short, long)]
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

    #[arg(long, hide = true)]
    registry_dir: Option<PathBuf>,

    /// Emit the baseline reference as JSON.
    #[arg(short, long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BaselineShowArgs {
    /// Benchmark identifier.
    benchmark_id: String,

    #[arg(long, hide = true)]
    registry_dir: Option<PathBuf>,

    /// Emit the baseline reference as JSON.
    #[arg(short, long)]
    json: bool,
}

fn main() -> Result<ExitCode> {
    let Cli { data_dir, command } = Cli::parse();
    let data = DataDirectory::new(data_dir);
    match command {
        Command::BenchmarkHost(args) => {
            benchmark_host::run_adapter(benchmark_host::AdapterOptions {
                root: args.root,
                benchmark: args.benchmark,
                fixture_lock: args.fixture_lock,
                bundle: args.bundle,
                bundle_metadata: args.bundle_metadata,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Doctor(args) => {
            let options = doctor::DoctorOptions {
                engines: args.engine.engines(),
                artifact_root: data.doctor(args.artifact_dir),
                runtime: BrowserInstallation::discover()?,
                json: args.json,
            };
            doctor::run(options)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Browsers(args) => match args.command {
            BrowsersCommand::Install(args) => {
                let runtime = BrowserInstallation::discover()?;
                runtime.install_browsers(&args.engine.browsers(), args.with_deps)?;
                Ok(ExitCode::SUCCESS)
            }
        },
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
                artifact_root: data.measurements(args.artifact_dir),
                json: args.json,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Measure(args) => {
            let outcome = runner::run(runner::MeasureOptions {
                benchmark: args.benchmark,
                variant: args.variant,
                sampling: runner::SamplingMode::Fixed(args.final_samples),
                artifact_root: data.measurements(args.artifact_dir),
                runtime: BrowserInstallation::discover()?,
            })?;
            outcome.report("measure", args.json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(mut args) => {
            let benchmark = match args.benchmark.take() {
                Some(path) if !path.is_dir() => path,
                requested_directory => {
                    if args.json || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                        bail!(
                            "interactive benchmark selection requires a terminal; \
                             pass a .bench.ts file to run non-interactively"
                        );
                    }
                    let directory =
                        requested_directory.unwrap_or_else(|| PathBuf::from("benchmarks"));
                    let Some(selection) = run_tui::select(run_tui::Options {
                        directory,
                        message: args.message.take(),
                        budget: args.execution.budget,
                        lineage_root: data.lineages(args.execution.lineage_dir.clone()),
                    })?
                    else {
                        return Ok(ExitCode::SUCCESS);
                    };
                    args.message = selection.message;
                    args.execution.budget = selection.budget;
                    selection.benchmark
                }
            };
            let outcome = managed_benchmark::run(managed_benchmark::RunOptions {
                benchmark,
                message: args.message,
                execution: args.execution.into_options(&data),
            })?;
            outcome.report(args.json)?;
            Ok(outcome.exit_code())
        }
        Command::Confirm(args) => {
            let outcome = managed_benchmark::confirm(managed_benchmark::ConfirmOptions {
                benchmark: args.benchmark,
                cycle_id: args.cycle_id,
                execution: args.execution.into_options(&data),
            })?;
            outcome.report(args.json)?;
            Ok(outcome.exit_code())
        }
        Command::Compare(args) => {
            let outcome = comparison::run(comparison::CompareOptions {
                candidate_root: args.candidate,
                baseline_root: args.baseline,
                registry_root: data.baselines(args.registry_dir),
                artifact_root: data.comparisons(args.artifact_dir),
                output: args.output,
            })?;
            outcome.report(args.json)?;
            Ok(outcome.exit_code())
        }
        Command::History(args) => {
            let interactive =
                args.is_bare() && io::stdin().is_terminal() && io::stdout().is_terminal();
            let root = data.lineages(args.lineage_dir);
            if interactive {
                history_tui::run(root)?;
            } else {
                lineage::history(lineage::HistoryOptions {
                    benchmark_id: args.benchmark_id,
                    root,
                    format: args.format.unwrap_or(HistoryFormatArg::Text).into(),
                })?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Show(args) => {
            lineage::show(lineage::ShowOptions {
                cycle_id: args.cycle_id,
                benchmark_id: args.benchmark,
                root: data.lineages(args.lineage_dir),
                diff: args.diff,
                json: args.json,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Accept(args) => {
            let outcome = lineage::accept(lineage::AcceptOptions {
                cycle_id: args.cycle_id,
                benchmark_id: args.benchmark,
                root: data.lineages(args.lineage_dir),
                registry_root: data.baselines(args.registry_dir),
            })?;
            outcome.report(args.json)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Baseline(args) => match args.command {
            BaselineCommand::Promote(args) => {
                baseline::promote(baseline::PromoteOptions {
                    measurement_root: args.measurement_set,
                    registry_root: data.baselines(args.registry_dir),
                    json: args.json,
                })?;
                Ok(ExitCode::SUCCESS)
            }
            BaselineCommand::Show(args) => {
                baseline::show(baseline::ShowOptions {
                    benchmark_id: args.benchmark_id,
                    registry_root: data.baselines(args.registry_dir),
                    json: args.json,
                })?;
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}
