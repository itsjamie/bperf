//! Append-only measured source, confirmation, and baseline-acceptance history.

use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as FmtWrite,
    fs,
    io::Write as IoWrite,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::{ArtifactKind, Engine};
use bperf_measurement::{manifest::VariantDescriptor, store::MeasurementSet};
use bperf_storage::database::{Database, DatabaseReader, WriteTransaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    baseline::{self, BaselineRecord},
    comparison::ComparisonSummary,
    environment::{self, EnvironmentSummary},
};

const SCHEMA_VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: usize = 4096;
const PROMOTION_CONFIRMATION_SEARCHES: usize = 5;
const LINEAGE_EVENTS: &str = "lineage";
const SOURCE_STATES: &str = "source_state";
const SOURCE_CHANGES: &str = "source_change";
const HISTORY_EVIDENCE: &str = "history_evidence";

pub struct RecordRunOptions {
    pub root: PathBuf,
    pub workspace_root: PathBuf,
    pub source_files: Vec<PathBuf>,
    pub measurement_root: PathBuf,
    /// Benchmark module path as invoked; recorded workspace-relative.
    pub benchmark_module: PathBuf,
    pub message: Option<String>,
    pub comparison: Option<ComparisonSummary>,
}

/// A recorded cycle together with its promotion readiness, computed from the
/// lineage state that includes the new cycle.
pub struct RecordedRun {
    pub cycle: CycleRecord,
    pub readiness: PromotionReadiness,
}

pub fn record_run(options: RecordRunOptions) -> Result<RecordedRun> {
    let measurement = MeasurementSet::open(&options.measurement_root)?;
    let environment_fingerprint = complete_environment(&measurement)?;
    if let Some(comparison) = &options.comparison {
        validate_comparison(&measurement, comparison)?;
    }

    let store = LineageStore::open(&options.root)?;
    let benchmark_module =
        workspace_relative_module(&options.workspace_root, &options.benchmark_module)?;
    let state = capture_measured_state(
        &store,
        &options.workspace_root,
        &options.source_files,
        &measurement,
    )?;
    let evidence = NewCycleEvidence {
        variant_id: measurement.variant_id().to_owned(),
        case_ids: measurement
            .benchmark()
            .workload_ids()
            .map(str::to_owned)
            .collect(),
        environment: environment::summary(&measurement)?,
        artifacts: retained_history_artifacts(&measurement),
    };
    let cycle = store.append_cycle_with_evidence(
        NewCycle {
            benchmark_id: measurement.benchmark_id().to_owned(),
            subject_id: measurement.subject_id().to_owned(),
            benchmark_sha256: measurement.benchmark_sha256().to_owned(),
            candidate_measurement_set: measurement.measurement_set_id().to_owned(),
            candidate_measurement_path: measurement.root().to_string_lossy().into_owned(),
            environment_fingerprint: environment_fingerprint.to_owned(),
            source_after: state,
            message: normalize_message(options.message)?,
            comparison: options.comparison,
            benchmark_module: Some(benchmark_module),
        },
        Some(evidence),
    )?;
    let events = store.read_events(&cycle.benchmark_id)?;
    let readiness = promotion_readiness(&cycle, &events);
    Ok(RecordedRun { cycle, readiness })
}

fn workspace_relative_module(workspace_root: &Path, module: &Path) -> Result<String> {
    let workspace_root = fs::canonicalize(workspace_root).with_context(|| {
        format!(
            "failed to resolve lineage workspace {}",
            workspace_root.display()
        )
    })?;
    let module = fs::canonicalize(module)
        .with_context(|| format!("failed to resolve benchmark module {}", module.display()))?;
    let relative = module.strip_prefix(&workspace_root).with_context(|| {
        format!(
            "benchmark module {} is outside workspace {}",
            module.display(),
            workspace_root.display()
        )
    })?;
    portable_path(relative)
}

pub struct ConfirmationTarget {
    cycle_id: String,
    benchmark_id: String,
    baseline_measurement_set: String,
    candidate_measurement_path: PathBuf,
}

impl ConfirmationTarget {
    pub fn cycle_id(&self) -> &str {
        &self.cycle_id
    }

    pub fn benchmark_id(&self) -> &str {
        &self.benchmark_id
    }

    pub fn baseline_measurement_set(&self) -> &str {
        &self.baseline_measurement_set
    }

    pub fn candidate_measurement_path(&self) -> &Path {
        &self.candidate_measurement_path
    }
}

pub fn confirmation_target(
    root: &Path,
    cycle_id: &str,
    benchmark_id: Option<&str>,
) -> Result<ConfirmationTarget> {
    let store = LineageStore::load(root)?;
    let (cycle, _, _) = store.find_cycle(cycle_id, benchmark_id)?;
    require_promotable(&cycle)?;
    let baseline_measurement_set = cycle
        .baseline_measurement_set
        .clone()
        .context("an initial baseline cycle does not need confirmation")?;
    Ok(ConfirmationTarget {
        cycle_id: cycle.cycle_id,
        benchmark_id: cycle.benchmark_id,
        baseline_measurement_set,
        candidate_measurement_path: PathBuf::from(cycle.candidate_measurement_path),
    })
}

pub struct RecordConfirmationOptions {
    pub root: PathBuf,
    pub cycle_id: String,
    pub workspace_root: PathBuf,
    pub source_files: Vec<PathBuf>,
    pub measurement_root: PathBuf,
    pub comparison: ComparisonSummary,
}

/// A recorded confirmation together with the confirmed cycle and its
/// promotion readiness after the confirmation was appended.
pub struct RecordedConfirmation {
    pub cycle: CycleRecord,
    pub confirmation: ConfirmationRecord,
    pub readiness: PromotionReadiness,
}

pub fn record_confirmation(options: RecordConfirmationOptions) -> Result<RecordedConfirmation> {
    require_cycle_id(&options.cycle_id)?;
    let measurement = MeasurementSet::open(&options.measurement_root)?;
    let environment_fingerprint = complete_environment(&measurement)?;
    validate_comparison(&measurement, &options.comparison)?;

    let store = LineageStore::open(&options.root)?;
    let (cycle, _, _) = store.find_cycle(&options.cycle_id, None)?;
    require_promotable(&cycle)?;
    let original = MeasurementSet::open(Path::new(&cycle.candidate_measurement_path))?;
    if original.benchmark_sha256() != measurement.benchmark_sha256()
        || original.variant_sha256() != measurement.variant_sha256()
    {
        bail!("confirmation does not measure the selected cycle's source variant");
    }
    let baseline_measurement_set = cycle
        .baseline_measurement_set
        .as_deref()
        .context("an initial baseline cycle does not need confirmation")?;
    if options.comparison.baseline_measurement_set != baseline_measurement_set {
        bail!("confirmation was not compared with the selected cycle's baseline");
    }
    let state = capture_measured_state(
        &store,
        &options.workspace_root,
        &options.source_files,
        &measurement,
    )?;
    if state.state_id != cycle.source_after {
        bail!("current source state does not match the selected optimization cycle");
    }
    let confirmation = store.append_confirmation(
        &cycle,
        measurement.measurement_set_id(),
        measurement.root(),
        environment_fingerprint,
        options.comparison,
    )?;
    let events = store.read_events(&cycle.benchmark_id)?;
    let readiness = promotion_readiness(&cycle, &events);
    Ok(RecordedConfirmation {
        cycle,
        confirmation,
        readiness,
    })
}

fn complete_environment(measurement: &MeasurementSet) -> Result<&str> {
    if !measurement.is_finalized() {
        bail!(
            "measurement set {} is incomplete and cannot enter optimization history",
            measurement.measurement_set_id()
        );
    }
    measurement
        .environment_fingerprint()
        .context("complete measurement set has no environment fingerprint")
}

fn validate_comparison(measurement: &MeasurementSet, comparison: &ComparisonSummary) -> Result<()> {
    comparison.validate_contract()?;
    if comparison.candidate_measurement_set != measurement.measurement_set_id() {
        bail!("comparison candidate does not match the measured source checkpoint");
    }
    if comparison.environment_fingerprint.as_deref() != measurement.environment_fingerprint() {
        bail!("comparison environment does not match the measured source checkpoint");
    }
    Ok(())
}

fn capture_measured_state(
    store: &LineageStore,
    workspace_root: &Path,
    source_files: &[PathBuf],
    measurement: &MeasurementSet,
) -> Result<SourceState> {
    let state = store.capture_state(workspace_root, source_files)?;
    let current_variant = VariantDescriptor::load(measurement.variant().source_path())
        .context("failed to verify the measured source state")?;
    if current_variant.source_sha256() != measurement.variant_sha256() {
        bail!(
            "source files changed while measurement set {} was running",
            measurement.measurement_set_id()
        );
    }
    Ok(state)
}

#[derive(Clone, Copy, Debug)]
pub enum HistoryFormat {
    Text,
    Json,
    AgentContext,
}

pub struct HistoryOptions {
    pub benchmark_id: Option<String>,
    pub root: PathBuf,
    pub format: HistoryFormat,
}

/// Lightweight benchmark choices for an interactive history client.
#[derive(Clone, Debug)]
pub struct HistoryIndex {
    pub benchmarks: Vec<HistoryIndexEntry>,
    pub latest_benchmark_id: String,
}

#[derive(Clone, Debug)]
pub struct HistoryIndexEntry {
    pub benchmark_id: String,
    pub cycle_count: usize,
    pub accepted_count: usize,
    pub latest_recorded_at_unix_ms: u64,
    pub latest_outcome: String,
    pub latest_message: Option<String>,
    pub current_baseline_label: Option<String>,
    pub latest_comparison: Option<ComparisonSummary>,
    pub wall_history_ms: BTreeMap<Engine, Vec<f64>>,
}

/// Compact lineage state suitable for the first interactive render.
///
/// Measurement sets and content-addressed payloads are not opened. Use
/// [`history_cycle`] to read the persisted evidence for one selected cycle.
#[derive(Clone, Debug)]
pub struct HistoryOverview {
    pub benchmark_id: String,
    pub subject_id: String,
    pub cycles: Vec<HistoryCycleSummary>,
    pub baselines: Vec<HistoryBaseline>,
    pub current_baseline_label: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HistoryCycleSummary {
    pub cycle_id: String,
    pub selector: String,
    pub recorded_at_unix_ms: u64,
    pub message: String,
    pub outcome: String,
    pub baseline_label: Option<String>,
    pub baseline_cycle_id: Option<String>,
    pub accepted_label: Option<String>,
    pub accepted: bool,
    pub current_baseline: bool,
    pub candidate_measurement_set: String,
    pub benchmark_module: Option<String>,
    pub comparison: Option<ComparisonSummary>,
    pub promotion: HistoryPromotionSummary,
}

/// Complete human-facing evidence for one benchmark's optimization lineage.
///
/// Storage events, native browser protocols, and measurement schemas stay
/// behind this snapshot. Artifact descriptors refer only to retained evidence;
/// payload bytes are read only when an artifact is explicitly opened.
#[derive(Clone, Debug)]
pub struct HistoryView {
    pub benchmark_id: String,
    pub subject_id: String,
    pub cycles: Vec<HistoryCycle>,
    pub baselines: Vec<HistoryBaseline>,
    pub current_baseline_label: Option<String>,
}

#[derive(Clone, Debug)]
pub struct HistoryBaseline {
    pub label: String,
    pub cycle_id: String,
    pub measurement_set_id: String,
    pub promoted_at_unix_ms: u64,
    pub current: bool,
}

#[derive(Clone, Debug)]
pub struct HistoryCycle {
    pub cycle_id: String,
    pub selector: String,
    pub recorded_at_unix_ms: u64,
    pub message: String,
    pub outcome: String,
    pub baseline_label: Option<String>,
    pub baseline_cycle_id: Option<String>,
    pub accepted_label: Option<String>,
    pub accepted: bool,
    pub current_baseline: bool,
    pub candidate_measurement_set: String,
    pub benchmark_module: Option<String>,
    pub variant_id: String,
    pub case_ids: Vec<String>,
    pub environment: EnvironmentSummary,
    pub comparison: Option<ComparisonSummary>,
    pub change: HistoryChangeSummary,
    pub promotion: HistoryPromotionSummary,
    pub artifacts: Vec<HistoryArtifact>,
}

impl HistoryCycle {
    pub fn accept_command(&self) -> String {
        render_accept_command(&self.selector)
    }

    pub fn confirm_command(&self) -> String {
        render_confirm_command(self.benchmark_module.as_deref(), &self.selector)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryChangeSummary {
    pub files_changed: usize,
    pub additions: usize,
    pub deletions: usize,
    pub binary_files: usize,
}

#[derive(Clone, Debug)]
pub struct HistoryPromotionSummary {
    pub ready: bool,
    pub confirmation_required: bool,
    pub searched_candidates: usize,
    pub search_threshold: usize,
    pub confirmations: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryArtifactKind {
    CpuProfile,
    Flamegraph,
    HeapSnapshot,
    Comparison,
    Sampling,
}

impl HistoryArtifactKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::CpuProfile => "cpu_profile",
            Self::Flamegraph => "flamegraph",
            Self::HeapSnapshot => "heap_snapshot",
            Self::Comparison => "comparison",
            Self::Sampling => "sampling",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryArtifact {
    pub kind: HistoryArtifactKind,
    pub engine: Option<Engine>,
    pub capture_scope: Option<String>,
    pub path: PathBuf,
}

/// Reusable history query session for interactive clients.
///
/// Each query uses a short database read and never opens native evidence
/// payloads.
pub struct HistoryReader {
    store: LineageStore,
    reader: DatabaseReader,
}

impl HistoryReader {
    pub fn open(root: &Path) -> Result<Self> {
        let store = LineageStore::load(root)?;
        let reader = store.database.reader()?;
        Ok(Self { store, reader })
    }

    pub fn index(&self) -> Result<HistoryIndex> {
        self.store.history_index(&self.reader)
    }

    pub fn overview(&self, benchmark_id: Option<&str>) -> Result<HistoryOverview> {
        let benchmark_id = self.benchmark_id(benchmark_id)?;
        let events = self.store.read_events_with(&self.reader, &benchmark_id)?;
        self.store.build_history_overview(&benchmark_id, &events)
    }

    pub fn cycle(&self, summary: &HistoryCycleSummary) -> Result<HistoryCycle> {
        require_cycle_id(&summary.cycle_id)?;
        self.store.load_history_cycle(&self.reader, summary.clone())
    }

    pub fn view(&self, benchmark_id: Option<&str>) -> Result<HistoryView> {
        let benchmark_id = self.benchmark_id(benchmark_id)?;
        let events = self.store.read_events_with(&self.reader, &benchmark_id)?;
        self.store
            .build_history_view(&self.reader, &benchmark_id, &events)
    }

    fn benchmark_id(&self, benchmark_id: Option<&str>) -> Result<String> {
        match benchmark_id {
            Some(benchmark_id) if benchmark_id != "latest" => {
                require_identifier("benchmark", benchmark_id)?;
                Ok(benchmark_id.to_owned())
            }
            Some(_) | None => Ok(self.index()?.latest_benchmark_id),
        }
    }
}

pub fn history_index(root: &Path) -> Result<HistoryIndex> {
    HistoryReader::open(root)?.index()
}

/// Returns the compact history index without treating an unused store as an
/// error. Invalid persisted history is still reported to the caller.
pub fn history_index_if_present(root: &Path) -> Result<Option<HistoryIndex>> {
    if !root.is_dir() {
        return Ok(None);
    }
    let reader = HistoryReader::open(root)?;
    reader.store.history_index_if_present(&reader.reader)
}

pub fn history_overview(root: &Path, benchmark_id: Option<&str>) -> Result<HistoryOverview> {
    HistoryReader::open(root)?.overview(benchmark_id)
}

/// Loads one cycle's compact persisted evidence without opening native payloads.
/// The cycle ID must be the full immutable ID, not a human selector prefix.
pub fn history_cycle(root: &Path, benchmark_id: &str, cycle_id: &str) -> Result<HistoryCycle> {
    require_identifier("benchmark", benchmark_id)?;
    require_cycle_id(cycle_id)?;
    let reader = HistoryReader::open(root)?;
    let overview = reader.overview(Some(benchmark_id))?;
    let summary = overview
        .cycles
        .into_iter()
        .find(|cycle| cycle.cycle_id == cycle_id)
        .with_context(|| format!("benchmark {benchmark_id:?} has no cycle {cycle_id}"))?;
    reader.cycle(&summary)
}

pub fn history_view(root: &Path, benchmark_id: Option<&str>) -> Result<HistoryView> {
    HistoryReader::open(root)?.view(benchmark_id)
}

pub fn history(options: HistoryOptions) -> Result<()> {
    let store = LineageStore::load(&options.root)?;
    let benchmark_id = match options.benchmark_id {
        Some(benchmark_id) if benchmark_id != "latest" => {
            require_identifier("benchmark", &benchmark_id)?;
            benchmark_id
        }
        Some(_) | None => store.latest_benchmark_id()?,
    };
    let events = store.read_events(&benchmark_id)?;
    match options.format {
        HistoryFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&HistoryReport {
                    schema_version: SCHEMA_VERSION,
                    benchmark_id: &benchmark_id,
                    events: &events,
                })?
            );
        }
        HistoryFormat::Text => print!("{}", store.render_history(&benchmark_id, &events)?),
        HistoryFormat::AgentContext => {
            print!("{}", store.render_agent_context(&benchmark_id, &events)?);
        }
    }
    Ok(())
}

pub struct ShowOptions {
    pub cycle_id: String,
    pub benchmark_id: Option<String>,
    pub root: PathBuf,
    pub diff: bool,
    pub json: bool,
}

pub fn show(options: ShowOptions) -> Result<()> {
    let store = LineageStore::load(&options.root)?;
    let (cycle, promotions, confirmations) =
        store.find_cycle(&options.cycle_id, options.benchmark_id.as_deref())?;
    if options.benchmark_id.is_none()
        && cycle_selector_prefix(&options.cycle_id)?.is_none()
        && let Some(notice) = store.crossover_notice(&cycle.benchmark_id)?
    {
        eprintln!("{notice}");
    }
    let events = store.read_events(&cycle.benchmark_id)?;
    let promotion_readiness = promotion_readiness(&cycle, &events);
    let reader = store.database.reader()?;
    let artifacts = store
        .read_cycle_evidence(&reader, &cycle.cycle_id)?
        .map(|evidence| evidence.artifacts);
    let diff = options
        .diff
        .then(|| store.render_change(&cycle.change_id))
        .transpose()?;
    if options.json {
        let change = options
            .diff
            .then(|| store.load_change(&cycle.change_id))
            .transpose()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&ShowReport {
                schema_version: SCHEMA_VERSION,
                cycle: &cycle,
                promotions: &promotions,
                confirmations: &confirmations,
                promotion_readiness: &promotion_readiness,
                artifacts: artifacts.as_deref(),
                change,
                diff,
            })?
        );
    } else {
        print!("{}", render_cycle(&cycle));
        println!(
            "  promotion readiness: {}",
            if !cycle.promotable() {
                "not promotable"
            } else if promotion_readiness.ready {
                "ready"
            } else {
                "confirmation required"
            }
        );
        for promotion in &promotions {
            writeln!(
                std::io::stdout(),
                "  accepted: {}",
                promotion.baseline_measurement_set
            )?;
        }
        for confirmation in &confirmations {
            writeln!(
                std::io::stdout(),
                "  confirmation: {} ({})",
                confirmation.confirmation_measurement_set,
                confirmation.outcome
            )?;
        }
        if let Some(artifacts) = &artifacts {
            print!("{}", render_artifacts(artifacts));
        }
        if promotions.is_empty()
            && let Some(next) = cycle.next_command(&promotion_readiness)
        {
            println!("  next: {next}");
        }
        if let Some(diff) = diff {
            print!("{diff}");
        }
    }
    Ok(())
}

pub struct AcceptOptions {
    pub cycle_id: String,
    pub benchmark_id: Option<String>,
    pub root: PathBuf,
    pub registry_root: PathBuf,
}

pub fn accept(options: AcceptOptions) -> Result<AcceptOutcome> {
    let store = LineageStore::load(&options.root)?;
    let (cycle, _, _) = store.find_cycle(&options.cycle_id, options.benchmark_id.as_deref())?;
    if options.benchmark_id.is_none()
        && cycle_selector_prefix(&options.cycle_id)?.is_none()
        && let Some(notice) = store.crossover_notice(&cycle.benchmark_id)?
    {
        eprintln!("{notice}");
    }
    let events = store.read_events(&cycle.benchmark_id)?;
    require_promotion_ready(&cycle, &events)?;
    let pending = baseline::prepare_measurement(Path::new(&cycle.candidate_measurement_path))?;
    if pending.benchmark_id() != cycle.benchmark_id {
        bail!("accepted measurement does not match the cycle benchmark");
    }
    let database = baseline::promotion_database(&options.registry_root)?;
    if !database.same_store(&store.database) {
        bail!(
            "baseline and lineage state must share one bperf data directory; use --data-dir instead of separate storage overrides"
        );
    }
    let (baseline, promotion) = database.write(|transaction| {
        let baseline = baseline::promote_prepared(transaction, &pending)?;
        let promotion = store.append_promotion(transaction, &cycle, &baseline)?;
        Ok((baseline, promotion))
    })?;
    Ok(AcceptOutcome {
        cycle,
        promotion,
        baseline,
    })
}

pub struct AcceptOutcome {
    cycle: CycleRecord,
    promotion: PromotionRecord,
    baseline: BaselineRecord,
}

impl AcceptOutcome {
    pub fn report(&self, json: bool) -> Result<()> {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&AcceptReport {
                    schema_version: SCHEMA_VERSION,
                    status: "accepted",
                    cycle: &self.cycle,
                    promotion: &self.promotion,
                    baseline: &self.baseline,
                })?
            );
        } else {
            print!("{}", self.render_text());
        }
        Ok(())
    }

    fn render_text(&self) -> String {
        let mut output = format!("bperf accept: {}\n", self.cycle.selector());
        let _ = writeln!(output, "  benchmark: {}", self.cycle.benchmark_id);
        let _ = writeln!(
            output,
            "  baseline measurement set: {}",
            self.baseline.measurement_set_id()
        );
        output
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CycleRecord {
    schema_version: u32,
    cycle_id: String,
    previous_cycle_id: Option<String>,
    recorded_at_unix_ms: u64,
    benchmark_id: String,
    subject_id: String,
    benchmark_sha256: String,
    message: Option<String>,
    source_before: Option<String>,
    source_after: String,
    change_id: String,
    baseline_measurement_set: Option<String>,
    candidate_measurement_set: String,
    candidate_measurement_path: String,
    environment_fingerprint: String,
    outcome: String,
    comparison: Option<ComparisonSummary>,
    /// Workspace-relative portable path of the measured benchmark module,
    /// e.g. `benchmarks/parser.bench.ts`. Absent in records written before
    /// this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    benchmark_module: Option<String>,
}

/// Outcomes that may be promoted to a baseline.
pub fn promotable_outcome(outcome: &str) -> bool {
    matches!(outcome, "measured" | "positive" | "equivalent")
}

impl CycleRecord {
    pub fn cycle_id(&self) -> &str {
        &self.cycle_id
    }

    pub fn selector(&self) -> &str {
        short_id(&self.cycle_id)
    }

    pub fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }

    pub fn outcome(&self) -> &str {
        &self.outcome
    }

    pub fn benchmark_module(&self) -> Option<&str> {
        self.benchmark_module.as_deref()
    }

    pub fn promotable(&self) -> bool {
        promotable_outcome(&self.outcome)
    }

    pub fn accept_command(&self) -> String {
        render_accept_command(self.selector())
    }

    /// Records without a stored module path fall back to a
    /// `<benchmark.bench.ts>` placeholder the caller must substitute.
    pub fn confirm_command(&self) -> String {
        render_confirm_command(self.benchmark_module.as_deref(), self.selector())
    }

    /// The command that advances this cycle toward promotion, or `None` when
    /// the outcome is not promotable.
    pub fn next_command(&self, readiness: &PromotionReadiness) -> Option<String> {
        self.promotable().then(|| {
            if readiness.ready {
                self.accept_command()
            } else {
                self.confirm_command()
            }
        })
    }
}

fn render_accept_command(selector: &str) -> String {
    format!("bperf accept {selector}")
}

fn render_confirm_command(benchmark_module: Option<&str>, selector: &str) -> String {
    let benchmark_module =
        benchmark_module.map_or_else(|| "<benchmark.bench.ts>".to_owned(), command_path_argument);
    format!("bperf confirm {benchmark_module} {selector}")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PromotionRecord {
    schema_version: u32,
    promotion_id: String,
    recorded_at_unix_ms: u64,
    benchmark_id: String,
    cycle_id: String,
    baseline_measurement_set: String,
    previous_baseline_measurement_set: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationRecord {
    schema_version: u32,
    confirmation_id: String,
    recorded_at_unix_ms: u64,
    benchmark_id: String,
    cycle_id: String,
    source_state: String,
    original_candidate_measurement_set: String,
    confirmation_measurement_set: String,
    confirmation_measurement_path: String,
    environment_fingerprint: String,
    outcome: String,
    comparison: ComparisonSummary,
}

impl ConfirmationRecord {
    pub fn confirmation_id(&self) -> &str {
        &self.confirmation_id
    }

    pub fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }

    pub fn outcome(&self) -> &str {
        &self.outcome
    }

    pub fn cycle_selector(&self) -> &str {
        short_id(&self.cycle_id)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "event", content = "record", rename_all = "snake_case")]
enum LineageEvent {
    Cycle(Box<CycleRecord>),
    Confirmation(Box<ConfirmationRecord>),
    Promotion(PromotionRecord),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceState {
    schema_version: u32,
    state_id: String,
    files: Vec<SourceFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    path: String,
    sha256: String,
    size_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceChange {
    schema_version: u32,
    change_id: String,
    source_before: Option<String>,
    source_after: String,
    files: Vec<FileChange>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileChange {
    path: String,
    kind: ChangeKind,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

struct NewCycle {
    benchmark_id: String,
    subject_id: String,
    benchmark_sha256: String,
    candidate_measurement_set: String,
    candidate_measurement_path: String,
    environment_fingerprint: String,
    source_after: SourceState,
    message: Option<String>,
    comparison: Option<ComparisonSummary>,
    benchmark_module: Option<String>,
}

struct NewCycleEvidence {
    variant_id: String,
    case_ids: Vec<String>,
    environment: EnvironmentSummary,
    artifacts: Vec<HistoryArtifact>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCycleEvidence {
    schema_version: u32,
    cycle_id: String,
    variant_id: String,
    case_ids: Vec<String>,
    environment: EnvironmentSummary,
    change: HistoryChangeSummary,
    artifacts: Vec<HistoryArtifact>,
}

struct LineageStore {
    root: PathBuf,
    database: Database,
}

impl LineageStore {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("failed to create lineage store {}", root.display()))?;
        fs::create_dir_all(root.join("objects"))
            .context("failed to create lineage object directory")?;
        Self::load(root)
    }

    fn load(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!(
                "no bperf optimization history in {}; run `bperf run <benchmark.bench.ts>` first",
                root.display()
            );
        }
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve lineage store {}", root.display()))?;
        let database = Database::for_collection(&root, "lineages")?;
        Ok(Self { root, database })
    }

    fn capture_state(
        &self,
        workspace_root: &Path,
        source_files: &[PathBuf],
    ) -> Result<SourceState> {
        let workspace_root = fs::canonicalize(workspace_root).with_context(|| {
            format!(
                "failed to resolve lineage workspace {}",
                workspace_root.display()
            )
        })?;
        let state = self.snapshot_state(&workspace_root, source_files)?;
        let confirmation = self.snapshot_state(&workspace_root, source_files)?;
        if state.state_id != confirmation.state_id {
            bail!("source files changed while their optimization checkpoint was captured");
        }
        self.database
            .publish_document(SOURCE_STATES, &state.state_id, &state)?;
        Ok(state)
    }

    fn snapshot_state(
        &self,
        workspace_root: &Path,
        source_files: &[PathBuf],
    ) -> Result<SourceState> {
        let mut files = BTreeMap::new();
        for source in source_files {
            let source = fs::canonicalize(source)
                .with_context(|| format!("failed to resolve source file {}", source.display()))?;
            let relative = source.strip_prefix(workspace_root).with_context(|| {
                format!(
                    "source file {} is outside workspace {}",
                    source.display(),
                    workspace_root.display()
                )
            })?;
            let relative = portable_path(relative)?;
            let content = fs::read(&source)
                .with_context(|| format!("failed to read source file {}", source.display()))?;
            let sha256 = sha256(&content);
            self.write_immutable(&self.object_path(&sha256), &content)?;
            if files
                .insert(
                    relative.clone(),
                    SourceFile {
                        path: relative.clone(),
                        sha256,
                        size_bytes: content.len() as u64,
                    },
                )
                .is_some()
            {
                bail!("source graph contains duplicate path {relative:?}");
            }
        }
        if files.is_empty() {
            bail!("source graph contains no project modules");
        }
        let files: Vec<_> = files.into_values().collect();
        let state_id = source_state_id(&files);
        Ok(SourceState {
            schema_version: SCHEMA_VERSION,
            state_id,
            files,
        })
    }

    #[cfg(test)]
    fn append_cycle(&self, cycle: NewCycle) -> Result<CycleRecord> {
        self.append_cycle_with_evidence(cycle, None)
    }

    fn append_cycle_with_evidence(
        &self,
        cycle: NewCycle,
        evidence: Option<NewCycleEvidence>,
    ) -> Result<CycleRecord> {
        require_identifier("benchmark", &cycle.benchmark_id)?;
        let events = self.read_events_if_present(&cycle.benchmark_id)?;
        let previous = events.iter().rev().find_map(|event| match event {
            LineageEvent::Cycle(record) => Some(record.as_ref()),
            LineageEvent::Confirmation(_) | LineageEvent::Promotion(_) => None,
        });
        let comparison_id = cycle
            .comparison
            .as_ref()
            .map(|comparison| comparison.comparison_id.as_str());
        if let Some(previous) = previous
            && previous.source_after == cycle.source_after.state_id
            && previous.candidate_measurement_set == cycle.candidate_measurement_set
            && previous
                .comparison
                .as_ref()
                .map(|item| item.comparison_id.as_str())
                == comparison_id
        {
            if let Some(evidence) = evidence {
                self.publish_cycle_evidence(previous, evidence)?;
            }
            return Ok(previous.clone());
        }

        let previous_state = previous
            .map(|record| self.load_state(&record.source_after))
            .transpose()?;
        let change = self.store_change(previous_state.as_ref(), &cycle.source_after)?;
        let previous_cycle_id = previous.map(|record| record.cycle_id.clone());
        let cycle_id = cycle_id(
            previous_cycle_id.as_deref(),
            &cycle.source_after.state_id,
            &cycle.candidate_measurement_set,
            comparison_id,
        );
        let baseline_measurement_set = cycle
            .comparison
            .as_ref()
            .map(|comparison| comparison.baseline_measurement_set.clone());
        let outcome = cycle.comparison.as_ref().map_or_else(
            || "measured".to_owned(),
            |comparison| comparison.verdict.clone(),
        );
        let record = CycleRecord {
            schema_version: SCHEMA_VERSION,
            cycle_id,
            previous_cycle_id,
            recorded_at_unix_ms: unix_time_ms()?,
            benchmark_id: cycle.benchmark_id,
            subject_id: cycle.subject_id,
            benchmark_sha256: cycle.benchmark_sha256,
            message: cycle.message,
            source_before: previous_state.map(|state| state.state_id),
            source_after: cycle.source_after.state_id,
            change_id: change.change_id,
            baseline_measurement_set,
            candidate_measurement_set: cycle.candidate_measurement_set,
            candidate_measurement_path: cycle.candidate_measurement_path,
            environment_fingerprint: cycle.environment_fingerprint,
            outcome,
            comparison: cycle.comparison,
            benchmark_module: cycle.benchmark_module,
        };
        let event = LineageEvent::Cycle(Box::new(record.clone()));
        if let Some(evidence) = evidence {
            let stored = self.build_cycle_evidence(&record, evidence)?;
            let event_payload = serde_json::to_vec(&event)?;
            let evidence_payload = serde_json::to_vec(&stored)?;
            self.database.write(|transaction| {
                transaction.publish_document(
                    HISTORY_EVIDENCE,
                    &record.cycle_id,
                    &evidence_payload,
                )?;
                transaction.append_event_if_unchanged(
                    LINEAGE_EVENTS,
                    &record.benchmark_id,
                    events.len(),
                    &event_payload,
                )?;
                Ok(())
            })?;
        } else {
            self.database.append_event_if_unchanged(
                LINEAGE_EVENTS,
                &record.benchmark_id,
                events.len(),
                &event,
            )?;
        }
        Ok(record)
    }

    fn build_cycle_evidence(
        &self,
        record: &CycleRecord,
        evidence: NewCycleEvidence,
    ) -> Result<StoredCycleEvidence> {
        Ok(StoredCycleEvidence {
            schema_version: SCHEMA_VERSION,
            cycle_id: record.cycle_id.clone(),
            variant_id: evidence.variant_id,
            case_ids: evidence.case_ids,
            environment: evidence.environment,
            change: self.summarize_change(&record.change_id)?,
            artifacts: evidence.artifacts,
        })
    }

    fn publish_cycle_evidence(
        &self,
        record: &CycleRecord,
        evidence: NewCycleEvidence,
    ) -> Result<()> {
        let stored = self.build_cycle_evidence(record, evidence)?;
        self.database
            .publish_document(HISTORY_EVIDENCE, &record.cycle_id, &stored)
    }

    fn append_promotion(
        &self,
        transaction: &mut WriteTransaction<'_>,
        cycle: &CycleRecord,
        baseline: &BaselineRecord,
    ) -> Result<PromotionRecord> {
        if baseline.measurement_set_id() != cycle.candidate_measurement_set {
            bail!("accepted baseline does not match cycle candidate");
        }
        let promotion_id = promotion_id(
            &cycle.cycle_id,
            baseline.measurement_set_id(),
            baseline.previous_measurement_set_id(),
        );
        let events: Vec<LineageEvent> =
            transaction.read_events(LINEAGE_EVENTS, &cycle.benchmark_id)?;
        if let Some(existing) = events.iter().find_map(|event| match event {
            LineageEvent::Promotion(record) if record.promotion_id == promotion_id => {
                Some(record.clone())
            }
            _ => None,
        }) {
            return Ok(existing);
        }
        let record = PromotionRecord {
            schema_version: SCHEMA_VERSION,
            promotion_id,
            recorded_at_unix_ms: unix_time_ms()?,
            benchmark_id: cycle.benchmark_id.clone(),
            cycle_id: cycle.cycle_id.clone(),
            baseline_measurement_set: baseline.measurement_set_id().to_owned(),
            previous_baseline_measurement_set: baseline
                .previous_measurement_set_id()
                .map(str::to_owned),
        };
        transaction.append_event(
            LINEAGE_EVENTS,
            &cycle.benchmark_id,
            &serde_json::to_vec(&LineageEvent::Promotion(record.clone()))?,
        )?;
        Ok(record)
    }

    fn append_confirmation(
        &self,
        cycle: &CycleRecord,
        measurement_set: &str,
        measurement_path: &Path,
        environment_fingerprint: &str,
        comparison: ComparisonSummary,
    ) -> Result<ConfirmationRecord> {
        let confirmation_id =
            confirmation_id(&cycle.cycle_id, measurement_set, &comparison.comparison_id);
        let events = self.read_events(&cycle.benchmark_id)?;
        if let Some(existing) = events.iter().find_map(|event| match event {
            LineageEvent::Confirmation(record) if record.confirmation_id == confirmation_id => {
                Some(record.as_ref().clone())
            }
            _ => None,
        }) {
            return Ok(existing);
        }
        let record = ConfirmationRecord {
            schema_version: SCHEMA_VERSION,
            confirmation_id,
            recorded_at_unix_ms: unix_time_ms()?,
            benchmark_id: cycle.benchmark_id.clone(),
            cycle_id: cycle.cycle_id.clone(),
            source_state: cycle.source_after.clone(),
            original_candidate_measurement_set: cycle.candidate_measurement_set.clone(),
            confirmation_measurement_set: measurement_set.to_owned(),
            confirmation_measurement_path: measurement_path.to_string_lossy().into_owned(),
            environment_fingerprint: environment_fingerprint.to_owned(),
            outcome: comparison.verdict.clone(),
            comparison,
        };
        self.database.append_event_if_unchanged(
            LINEAGE_EVENTS,
            &cycle.benchmark_id,
            events.len(),
            &LineageEvent::Confirmation(Box::new(record.clone())),
        )?;
        Ok(record)
    }

    fn store_change(
        &self,
        before: Option<&SourceState>,
        after: &SourceState,
    ) -> Result<SourceChange> {
        let before_files: BTreeMap<_, _> = before
            .map(|state| {
                state
                    .files
                    .iter()
                    .map(|file| (file.path.as_str(), file))
                    .collect()
            })
            .unwrap_or_default();
        let after_files: BTreeMap<_, _> = after
            .files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect();
        let paths: std::collections::BTreeSet<_> = before_files
            .keys()
            .chain(after_files.keys())
            .copied()
            .collect();
        let files = paths
            .into_iter()
            .filter_map(|path| {
                let before = before_files.get(path);
                let after = after_files.get(path);
                if before.map(|file| &file.sha256) == after.map(|file| &file.sha256) {
                    return None;
                }
                Some(FileChange {
                    path: path.to_owned(),
                    kind: match (before, after) {
                        (None, Some(_)) => ChangeKind::Added,
                        (Some(_), None) => ChangeKind::Deleted,
                        (Some(_), Some(_)) => ChangeKind::Modified,
                        (None, None) => unreachable!(),
                    },
                    before_sha256: before.map(|file| file.sha256.clone()),
                    after_sha256: after.map(|file| file.sha256.clone()),
                })
            })
            .collect();
        let source_before = before.map(|state| state.state_id.clone());
        let change_id = change_id(source_before.as_deref(), &after.state_id);
        let change = SourceChange {
            schema_version: SCHEMA_VERSION,
            change_id,
            source_before,
            source_after: after.state_id.clone(),
            files,
        };
        self.database
            .publish_document(SOURCE_CHANGES, &change.change_id, &change)?;
        Ok(change)
    }

    fn read_events(&self, benchmark_id: &str) -> Result<Vec<LineageEvent>> {
        let reader = self.database.reader()?;
        self.read_events_with(&reader, benchmark_id)
    }

    fn read_events_with(
        &self,
        reader: &DatabaseReader,
        benchmark_id: &str,
    ) -> Result<Vec<LineageEvent>> {
        let events = reader.read_events(LINEAGE_EVENTS, benchmark_id)?;
        if events.is_empty() {
            bail!("no optimization history for benchmark {benchmark_id:?}");
        }
        validate_events(self.database.path(), events, benchmark_id)
    }

    fn read_events_if_present(&self, benchmark_id: &str) -> Result<Vec<LineageEvent>> {
        let events = self.database.read_events(LINEAGE_EVENTS, benchmark_id)?;
        if events.is_empty() {
            return Ok(events);
        }
        validate_events(self.database.path(), events, benchmark_id)
    }

    #[cfg(test)]
    fn append_event(&self, benchmark_id: &str, event: &LineageEvent) -> Result<()> {
        self.database
            .append_event(LINEAGE_EVENTS, benchmark_id, event)
            .map(|_| ())
    }

    fn find_cycle(
        &self,
        selector: &str,
        benchmark_id: Option<&str>,
    ) -> Result<(CycleRecord, Vec<PromotionRecord>, Vec<ConfirmationRecord>)> {
        let prefix = cycle_selector_prefix(selector)?;
        let streams = match benchmark_id {
            Some(benchmark_id) => {
                require_identifier("benchmark", benchmark_id)?;
                vec![benchmark_id.to_owned()]
            }
            None => self.database.streams(LINEAGE_EVENTS)?,
        };
        let mut matches = Vec::new();
        for stream in &streams {
            let events = self.read_events(stream)?;
            for cycle in events.iter().filter_map(|event| match event {
                LineageEvent::Cycle(record) => Some(record.as_ref()),
                LineageEvent::Confirmation(_) | LineageEvent::Promotion(_) => None,
            }) {
                if prefix
                    .as_ref()
                    .is_none_or(|prefix| cycle.cycle_id.starts_with(prefix))
                {
                    matches.push(cycle.clone());
                }
            }
        }

        let cycle = if prefix.is_none() {
            matches
                .into_iter()
                .max_by(|left, right| {
                    left.recorded_at_unix_ms
                        .cmp(&right.recorded_at_unix_ms)
                        .then_with(|| left.cycle_id.cmp(&right.cycle_id))
                })
                .context(
                    "no measured optimization cycles; run `bperf run <benchmark.bench.ts>` first",
                )?
        } else {
            match matches.len() {
                0 => match benchmark_id {
                    Some(benchmark_id) => bail!(
                        "no optimization cycle matches {selector:?} for benchmark \
                         {benchmark_id:?}; run `bperf history {benchmark_id}` to list cycles"
                    ),
                    None => bail!(
                        "no optimization cycle matches {selector:?}; run `bperf history` to list cycles"
                    ),
                },
                1 => matches.pop().expect("one cycle selector match"),
                _ => {
                    let choices = matches
                        .iter()
                        .map(CycleRecord::selector)
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "cycle selector {selector:?} is ambiguous; use more of the ID ({choices})"
                    )
                }
            }
        };
        let events = self.read_events(&cycle.benchmark_id)?;
        let promotions = events
            .iter()
            .filter_map(|event| match event {
                LineageEvent::Promotion(record) if record.cycle_id == cycle.cycle_id => {
                    Some(record.clone())
                }
                _ => None,
            })
            .collect();
        let confirmations = events
            .into_iter()
            .filter_map(|event| match event {
                LineageEvent::Confirmation(record) if record.cycle_id == cycle.cycle_id => {
                    Some(record.as_ref().clone())
                }
                _ => None,
            })
            .collect();
        Ok((cycle, promotions, confirmations))
    }

    fn latest_benchmark_id(&self) -> Result<String> {
        Ok(self.find_cycle("latest", None)?.0.benchmark_id)
    }

    /// A stderr notice for commands that resolved an unscoped `latest` in a
    /// store holding more than one benchmark stream. `None` when the store
    /// has a single stream and the resolution cannot surprise.
    fn crossover_notice(&self, resolved: &str) -> Result<Option<String>> {
        let benchmarks = self.database.streams(LINEAGE_EVENTS)?;
        Ok(crossover_notice(resolved, &benchmarks))
    }

    fn history_index(&self, reader: &DatabaseReader) -> Result<HistoryIndex> {
        self.history_index_if_present(reader)?
            .context("no measured optimization cycles; run `bperf run <benchmark.bench.ts>` first")
    }

    fn history_index_if_present(&self, reader: &DatabaseReader) -> Result<Option<HistoryIndex>> {
        let mut benchmarks = Vec::new();
        for benchmark_id in reader.streams(LINEAGE_EVENTS)? {
            let events = self.read_events_with(reader, &benchmark_id)?;
            let mut cycles = events
                .iter()
                .filter_map(|event| match event {
                    LineageEvent::Cycle(cycle) => Some(cycle.as_ref()),
                    LineageEvent::Confirmation(_) | LineageEvent::Promotion(_) => None,
                })
                .collect::<Vec<_>>();
            let Some(latest) = cycles.iter().copied().max_by(|left, right| {
                left.recorded_at_unix_ms
                    .cmp(&right.recorded_at_unix_ms)
                    .then_with(|| left.cycle_id.cmp(&right.cycle_id))
            }) else {
                continue;
            };
            let timeline = baseline_timeline(&cycles, &events)?;
            cycles.sort_by(|left, right| {
                left.recorded_at_unix_ms
                    .cmp(&right.recorded_at_unix_ms)
                    .then_with(|| left.cycle_id.cmp(&right.cycle_id))
            });
            let mut wall_history_ms = BTreeMap::new();
            for engine in Engine::ALL {
                let history = cycles
                    .iter()
                    .filter_map(|cycle| {
                        cycle
                            .comparison
                            .as_ref()
                            .and_then(|comparison| {
                                comparison
                                    .engines
                                    .iter()
                                    .find(|summary| summary.engine == engine)
                            })
                            .and_then(|summary| summary.metrics.get("workload.wall_ms"))
                            .and_then(|metric| metric.candidate_value)
                    })
                    .collect();
                wall_history_ms.insert(engine, history);
            }
            benchmarks.push(HistoryIndexEntry {
                benchmark_id,
                cycle_count: cycles.len(),
                accepted_count: timeline.baselines.len(),
                latest_recorded_at_unix_ms: latest.recorded_at_unix_ms,
                latest_outcome: latest.outcome.clone(),
                latest_message: latest.message.as_deref().map(one_line),
                current_baseline_label: timeline.current_baseline_label.clone(),
                latest_comparison: latest.comparison.clone(),
                wall_history_ms,
            });
        }
        benchmarks.sort_by(|left, right| {
            right
                .latest_recorded_at_unix_ms
                .cmp(&left.latest_recorded_at_unix_ms)
                .then_with(|| left.benchmark_id.cmp(&right.benchmark_id))
        });
        let Some(latest_benchmark_id) = benchmarks.first().map(|entry| entry.benchmark_id.clone())
        else {
            return Ok(None);
        };
        Ok(Some(HistoryIndex {
            benchmarks,
            latest_benchmark_id,
        }))
    }

    fn build_history_overview(
        &self,
        benchmark_id: &str,
        events: &[LineageEvent],
    ) -> Result<HistoryOverview> {
        let cycle_records = events
            .iter()
            .filter_map(|event| match event {
                LineageEvent::Cycle(cycle) => Some(cycle.as_ref()),
                LineageEvent::Confirmation(_) | LineageEvent::Promotion(_) => None,
            })
            .collect::<Vec<_>>();
        let subject_id = cycle_records
            .first()
            .map(|cycle| cycle.subject_id.clone())
            .context("optimization history has no measured cycles")?;

        let cycle_by_measurement = cycle_records
            .iter()
            .map(|cycle| (cycle.candidate_measurement_set.as_str(), *cycle))
            .collect::<BTreeMap<_, _>>();
        let timeline = baseline_timeline(&cycle_records, events)?;

        let mut cycles = Vec::with_capacity(cycle_records.len());
        for cycle in cycle_records {
            let readiness = promotion_readiness(cycle, events);
            let baseline_cycle_id = cycle
                .baseline_measurement_set
                .as_deref()
                .and_then(|measurement| cycle_by_measurement.get(measurement))
                .map(|cycle| cycle.cycle_id.clone());
            let confirmations = events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        LineageEvent::Confirmation(confirmation)
                            if confirmation.cycle_id == cycle.cycle_id
                    )
                })
                .count();
            cycles.push(HistoryCycleSummary {
                cycle_id: cycle.cycle_id.clone(),
                selector: cycle.selector().to_owned(),
                recorded_at_unix_ms: cycle.recorded_at_unix_ms,
                message: cycle
                    .message
                    .as_deref()
                    .map(one_line)
                    .unwrap_or_else(|| "(no message)".to_owned()),
                outcome: cycle.outcome.clone(),
                baseline_label: cycle
                    .baseline_measurement_set
                    .as_deref()
                    .and_then(|measurement| timeline.label_by_measurement.get(measurement))
                    .cloned(),
                baseline_cycle_id,
                accepted_label: timeline
                    .label_by_cycle
                    .get(cycle.cycle_id.as_str())
                    .cloned(),
                accepted: timeline
                    .label_by_cycle
                    .contains_key(cycle.cycle_id.as_str()),
                current_baseline: timeline.current_baseline_cycle.as_deref()
                    == Some(cycle.cycle_id.as_str()),
                candidate_measurement_set: cycle.candidate_measurement_set.clone(),
                benchmark_module: cycle.benchmark_module.clone(),
                comparison: cycle.comparison.clone(),
                promotion: HistoryPromotionSummary {
                    ready: readiness.ready,
                    confirmation_required: readiness.confirmation_required,
                    searched_candidates: readiness.searched_candidates,
                    search_threshold: readiness.search_threshold,
                    confirmations,
                },
            });
        }
        cycles.sort_by(|left, right| {
            right
                .recorded_at_unix_ms
                .cmp(&left.recorded_at_unix_ms)
                .then_with(|| right.cycle_id.cmp(&left.cycle_id))
        });
        let current_baseline_label = timeline.current_baseline_label;
        let mut baselines = timeline.baselines;
        baselines.reverse();
        Ok(HistoryOverview {
            benchmark_id: benchmark_id.to_owned(),
            subject_id,
            cycles,
            baselines,
            current_baseline_label,
        })
    }

    fn build_history_view(
        &self,
        reader: &DatabaseReader,
        benchmark_id: &str,
        events: &[LineageEvent],
    ) -> Result<HistoryView> {
        let HistoryOverview {
            benchmark_id,
            subject_id,
            cycles: summaries,
            baselines,
            current_baseline_label,
        } = self.build_history_overview(benchmark_id, events)?;
        let cycles = summaries
            .into_iter()
            .map(|summary| self.load_history_cycle(reader, summary))
            .collect::<Result<_>>()?;
        Ok(HistoryView {
            benchmark_id,
            subject_id,
            cycles,
            baselines,
            current_baseline_label,
        })
    }

    /// Reads one cycle's persisted evidence document. `None` means the cycle
    /// predates evidence persistence; an incompatible schema or mismatched
    /// identity is an error.
    fn read_cycle_evidence(
        &self,
        reader: &DatabaseReader,
        cycle_id: &str,
    ) -> Result<Option<StoredCycleEvidence>> {
        let Some(stored) =
            reader.read_document::<StoredCycleEvidence>(HISTORY_EVIDENCE, cycle_id)?
        else {
            return Ok(None);
        };
        if stored.schema_version != SCHEMA_VERSION || stored.cycle_id != cycle_id {
            bail!(
                "history evidence for {} has incompatible identity",
                short_id(cycle_id)
            );
        }
        Ok(Some(stored))
    }

    fn load_history_cycle(
        &self,
        reader: &DatabaseReader,
        summary: HistoryCycleSummary,
    ) -> Result<HistoryCycle> {
        let stored = self
            .read_cycle_evidence(reader, &summary.cycle_id)?
            .with_context(|| {
                format!(
                    "cycle {} has no persisted history evidence",
                    summary.selector
                )
            })?;
        let StoredCycleEvidence {
            schema_version: _,
            cycle_id: _,
            variant_id,
            case_ids,
            environment,
            change,
            artifacts,
        } = stored;
        let HistoryCycleSummary {
            cycle_id,
            selector,
            recorded_at_unix_ms,
            message,
            outcome,
            baseline_label,
            baseline_cycle_id,
            accepted_label,
            accepted,
            current_baseline,
            candidate_measurement_set,
            benchmark_module,
            comparison,
            promotion,
        } = summary;
        Ok(HistoryCycle {
            cycle_id,
            selector,
            recorded_at_unix_ms,
            message,
            outcome,
            baseline_label,
            baseline_cycle_id,
            accepted_label,
            accepted,
            current_baseline,
            candidate_measurement_set,
            benchmark_module,
            variant_id,
            case_ids,
            environment,
            comparison,
            change,
            promotion,
            artifacts,
        })
    }

    fn summarize_change(&self, change_id: &str) -> Result<HistoryChangeSummary> {
        let change = self.load_change(change_id)?;
        let mut additions = 0;
        let mut deletions = 0;
        let mut binary_files = 0;
        for file in &change.files {
            let before = file
                .before_sha256
                .as_deref()
                .map(|digest| self.read_object(digest, &file.path))
                .transpose()?
                .unwrap_or_default();
            let after = file
                .after_sha256
                .as_deref()
                .map(|digest| self.read_object(digest, &file.path))
                .transpose()?
                .unwrap_or_default();
            match (std::str::from_utf8(&before), std::str::from_utf8(&after)) {
                (Ok(before), Ok(after)) => {
                    let old = before.lines().collect::<Vec<_>>();
                    let new = after.lines().collect::<Vec<_>>();
                    let (prefix, old_change_end, new_change_end) = changed_span(&old, &new);
                    deletions += old_change_end.saturating_sub(prefix);
                    additions += new_change_end.saturating_sub(prefix);
                }
                _ => binary_files += 1,
            }
        }
        Ok(HistoryChangeSummary {
            files_changed: change.files.len(),
            additions,
            deletions,
            binary_files,
        })
    }

    fn render_history(&self, benchmark_id: &str, events: &[LineageEvent]) -> Result<String> {
        let measurements = history_measurements(events)?;
        let current_baseline = events.iter().rev().find_map(|event| match event {
            LineageEvent::Promotion(promotion) => Some(promotion.cycle_id.as_str()),
            LineageEvent::Cycle(_) | LineageEvent::Confirmation(_) => None,
        });
        let mut output = format!(
            "bperf history: {benchmark_id} ({} runs)\n",
            measurements.len()
        );
        for case_id in history_case_ids(&measurements) {
            writeln!(output, "\ncase: {case_id}")?;
            writeln!(output, "  {:<20}  message", "cycle")?;
            for item in measurements
                .iter()
                .filter(|item| item.measurement.benchmark().workload(&case_id).is_some())
            {
                let message = item
                    .cycle
                    .message
                    .as_deref()
                    .map(one_line)
                    .unwrap_or_else(|| "(no message)".to_owned());
                let baseline = if current_baseline == Some(item.cycle.cycle_id()) {
                    " [baseline]"
                } else {
                    ""
                };
                writeln!(
                    output,
                    "  {:<20}  {message}{baseline}",
                    item.cycle.selector()
                )?;
            }
        }
        Ok(output)
    }

    fn render_agent_context(&self, benchmark_id: &str, events: &[LineageEvent]) -> Result<String> {
        let cycles: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                LineageEvent::Cycle(cycle) => Some(cycle),
                LineageEvent::Confirmation(_) | LineageEvent::Promotion(_) => None,
            })
            .collect();
        let mut output = format!(
            "# bperf optimization history\n\nBenchmark: `{benchmark_id}`\nMeasured cycles: {}\n\n",
            cycles.len()
        );
        for (index, cycle) in cycles.into_iter().enumerate() {
            writeln!(
                output,
                "## {}. `{}` — {}",
                index + 1,
                cycle.cycle_id,
                cycle.outcome
            )?;
            if let Some(message) = &cycle.message {
                writeln!(output, "\nHypothesis: {message}")?;
            }
            output.push('\n');
            output.push_str(&render_engine_results(cycle));
            let readiness = promotion_readiness(cycle, events);
            if readiness.confirmation_required {
                writeln!(
                    output,
                    "\nPromotion confirmation: {} after {} searched candidates.",
                    if readiness.ready {
                        "satisfied"
                    } else {
                        "required"
                    },
                    readiness.searched_candidates
                )?;
            }
            if cycle.source_before.is_some() {
                writeln!(output, "\n```diff")?;
                output.push_str(&self.render_change(&cycle.change_id)?);
                writeln!(output, "```\n")?;
            } else {
                writeln!(
                    output,
                    "\nInitial measured source checkpoint: `{}`\n",
                    cycle.source_after
                )?;
            }
        }
        let promotions: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                LineageEvent::Promotion(record) => Some(record),
                LineageEvent::Cycle(_) | LineageEvent::Confirmation(_) => None,
            })
            .collect();
        let confirmations: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                LineageEvent::Confirmation(record) => Some(record),
                LineageEvent::Cycle(_) | LineageEvent::Promotion(_) => None,
            })
            .collect();
        if !confirmations.is_empty() {
            output.push_str("## Confirmation measurements\n\n");
            for confirmation in confirmations {
                writeln!(
                    output,
                    "- `{}` confirmed `{}` as {} with `{}`",
                    confirmation.confirmation_id,
                    confirmation.cycle_id,
                    confirmation.outcome,
                    confirmation.confirmation_measurement_set
                )?;
            }
            output.push('\n');
        }
        if !promotions.is_empty() {
            output.push_str("## Baseline promotions\n\n");
            for promotion in promotions {
                writeln!(
                    output,
                    "- `{}` accepted `{}` as `{}`",
                    promotion.promotion_id, promotion.cycle_id, promotion.baseline_measurement_set
                )?;
            }
        }
        Ok(output)
    }

    fn render_change(&self, change_id: &str) -> Result<String> {
        let change = self.load_change(change_id)?;
        let mut output = String::new();
        for file in &change.files {
            let before = file
                .before_sha256
                .as_deref()
                .map(|digest| self.read_object(digest, &file.path))
                .transpose()?
                .unwrap_or_default();
            let after = file
                .after_sha256
                .as_deref()
                .map(|digest| self.read_object(digest, &file.path))
                .transpose()?
                .unwrap_or_default();
            output.push_str(&file_diff(
                &file.path,
                &before,
                &after,
                file.before_sha256.as_deref(),
                file.after_sha256.as_deref(),
            ));
        }
        if output.is_empty() {
            output.push_str("(no source changes)\n");
        }
        Ok(output)
    }

    fn load_state(&self, state_id: &str) -> Result<SourceState> {
        require_hash_id("source state", state_id, "state-")?;
        let state: SourceState = self
            .database
            .read_document(SOURCE_STATES, state_id)?
            .with_context(|| format!("failed to read source state {state_id}"))?;
        if state.schema_version != SCHEMA_VERSION || state.state_id != state_id {
            bail!("source state {state_id} has incompatible identity");
        }
        for file in &state.files {
            require_digest(&file.sha256)?;
            require_portable_file_path(&file.path)?;
        }
        Ok(state)
    }

    fn load_change(&self, change_id: &str) -> Result<SourceChange> {
        require_hash_id("source change", change_id, "change-")?;
        let change: SourceChange = self
            .database
            .read_document(SOURCE_CHANGES, change_id)?
            .with_context(|| format!("failed to read source change {change_id}"))?;
        if change.schema_version != SCHEMA_VERSION || change.change_id != change_id {
            bail!("source change {change_id} has incompatible identity");
        }
        if let Some(source_before) = &change.source_before {
            require_hash_id("source state", source_before, "state-")?;
        }
        require_hash_id("source state", &change.source_after, "state-")?;
        for file in &change.files {
            require_portable_file_path(&file.path)?;
            if let Some(digest) = &file.before_sha256 {
                require_digest(digest)?;
            }
            if let Some(digest) = &file.after_sha256 {
                require_digest(digest)?;
            }
        }
        Ok(change)
    }

    fn read_object(&self, digest: &str, source_path: &str) -> Result<Vec<u8>> {
        require_digest(digest)?;
        let content = fs::read(self.object_path(digest))
            .with_context(|| format!("failed to read source object for {source_path}"))?;
        if sha256(&content) != digest {
            bail!("source object for {source_path} failed its content digest");
        }
        Ok(content)
    }

    fn write_immutable(&self, path: &Path, content: &[u8]) -> Result<()> {
        bperf_storage::publish_immutable(path, content)
            .with_context(|| format!("failed to store lineage object {}", path.display()))
    }

    fn object_path(&self, sha256: &str) -> PathBuf {
        self.root.join("objects").join(sha256)
    }
}

struct HistoryMeasurement<'a> {
    cycle: &'a CycleRecord,
    measurement: MeasurementSet,
}

struct BaselineTimeline {
    baselines: Vec<HistoryBaseline>,
    label_by_cycle: BTreeMap<String, String>,
    label_by_measurement: BTreeMap<String, String>,
    current_baseline_cycle: Option<String>,
    current_baseline_label: Option<String>,
}

fn baseline_timeline(cycles: &[&CycleRecord], events: &[LineageEvent]) -> Result<BaselineTimeline> {
    let cycle_by_measurement = cycles
        .iter()
        .map(|cycle| (cycle.candidate_measurement_set.as_str(), *cycle))
        .collect::<BTreeMap<_, _>>();
    let cycle_by_id = cycles
        .iter()
        .map(|cycle| (cycle.cycle_id.as_str(), *cycle))
        .collect::<BTreeMap<_, _>>();
    let mut baseline_roles = BTreeMap::new();
    let mut current_baseline_cycle = None;
    for event in events {
        match event {
            LineageEvent::Cycle(cycle) => {
                let Some(parent) = cycle
                    .baseline_measurement_set
                    .as_deref()
                    .and_then(|measurement| cycle_by_measurement.get(measurement))
                else {
                    continue;
                };
                baseline_roles.entry(parent.cycle_id.as_str()).or_insert((
                    cycle.recorded_at_unix_ms,
                    parent.candidate_measurement_set.as_str(),
                ));
                current_baseline_cycle = Some(parent.cycle_id.as_str());
            }
            LineageEvent::Promotion(promotion) => {
                let promoted_cycle = cycle_by_id
                    .get(promotion.cycle_id.as_str())
                    .context("promotion refers to an unknown measured cycle")?;
                baseline_roles
                    .entry(promoted_cycle.cycle_id.as_str())
                    .and_modify(|(recorded_at, _)| {
                        *recorded_at = (*recorded_at).min(promotion.recorded_at_unix_ms);
                    })
                    .or_insert((
                        promotion.recorded_at_unix_ms,
                        promoted_cycle.candidate_measurement_set.as_str(),
                    ));
                current_baseline_cycle = Some(promoted_cycle.cycle_id.as_str());
            }
            LineageEvent::Confirmation(_) => {}
        }
    }
    let mut ordered_roles = baseline_roles
        .into_iter()
        .map(|(cycle_id, (recorded_at, measurement_set_id))| {
            (cycle_id, measurement_set_id, recorded_at)
        })
        .collect::<Vec<_>>();
    ordered_roles.sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(right.0)));

    let mut label_by_cycle = BTreeMap::new();
    let mut baselines = Vec::new();
    for (cycle_id, measurement_set_id, recorded_at_unix_ms) in ordered_roles {
        let label = format!("b-{:02}", baselines.len() + 1);
        label_by_cycle.insert(cycle_id.to_owned(), label.clone());
        baselines.push(HistoryBaseline {
            label,
            cycle_id: cycle_id.to_owned(),
            measurement_set_id: measurement_set_id.to_owned(),
            promoted_at_unix_ms: recorded_at_unix_ms,
            current: false,
        });
    }
    for baseline in &mut baselines {
        baseline.current = current_baseline_cycle == Some(baseline.cycle_id.as_str());
    }
    let label_by_measurement = baselines
        .iter()
        .map(|baseline| (baseline.measurement_set_id.clone(), baseline.label.clone()))
        .collect::<BTreeMap<_, _>>();
    let current_baseline_cycle = current_baseline_cycle.map(str::to_owned);
    let current_baseline_label = current_baseline_cycle
        .as_deref()
        .and_then(|cycle_id| label_by_cycle.get(cycle_id))
        .cloned();
    Ok(BaselineTimeline {
        baselines,
        label_by_cycle,
        label_by_measurement,
        current_baseline_cycle,
        current_baseline_label,
    })
}

fn retained_history_artifacts(measurement: &MeasurementSet) -> Vec<HistoryArtifact> {
    let mut paths = HashSet::new();
    let mut artifacts = Vec::new();
    for (engine, artifact) in measurement.retained_artifacts() {
        let path = measurement.root().join(&artifact.path);
        if !paths.insert(path.clone()) {
            continue;
        }
        let kind = match artifact.kind {
            ArtifactKind::CpuProfile => HistoryArtifactKind::CpuProfile,
            ArtifactKind::JsHeap => HistoryArtifactKind::HeapSnapshot,
            ArtifactKind::Flamegraph => HistoryArtifactKind::Flamegraph,
        };
        artifacts.push(HistoryArtifact {
            kind,
            engine: Some(engine),
            capture_scope: Some(artifact.capture_scope.clone()),
            path,
        });
    }
    artifacts.sort_by(|left, right| {
        (
            left.engine.map(engine_rank).unwrap_or(Engine::ALL.len()),
            left.kind,
            left.capture_scope.as_deref(),
            &left.path,
        )
            .cmp(&(
                right.engine.map(engine_rank).unwrap_or(Engine::ALL.len()),
                right.kind,
                right.capture_scope.as_deref(),
                &right.path,
            ))
    });
    artifacts
}

fn engine_rank(engine: Engine) -> usize {
    Engine::ALL
        .iter()
        .position(|candidate| *candidate == engine)
        .expect("required engine has a stable display position")
}

fn history_measurements(events: &[LineageEvent]) -> Result<Vec<HistoryMeasurement<'_>>> {
    events
        .iter()
        .filter_map(|event| match event {
            LineageEvent::Cycle(cycle) => Some(cycle.as_ref()),
            LineageEvent::Confirmation(_) | LineageEvent::Promotion(_) => None,
        })
        .map(|cycle| {
            let measurement = MeasurementSet::open(Path::new(&cycle.candidate_measurement_path))
                .with_context(|| {
                    format!(
                        "failed to read measurement for history cycle {}",
                        cycle.selector()
                    )
                })?;
            Ok(HistoryMeasurement { cycle, measurement })
        })
        .collect()
}

fn history_case_ids(measurements: &[HistoryMeasurement<'_>]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut cases = Vec::new();
    for item in measurements {
        for case_id in item.measurement.benchmark().workload_ids() {
            if seen.insert(case_id.to_owned()) {
                cases.push(case_id.to_owned());
            }
        }
    }
    cases
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_events(
    path: &Path,
    events: Vec<LineageEvent>,
    benchmark_id: &str,
) -> Result<Vec<LineageEvent>> {
    let mut last_cycle: Option<String> = None;
    let mut last_source: Option<String> = None;
    let mut cycles = BTreeMap::new();
    let mut confirmations = HashSet::new();
    let mut promotions = HashSet::new();
    for (event_index, event) in events.iter().enumerate() {
        match event {
            LineageEvent::Cycle(cycle) => {
                if cycle.schema_version != SCHEMA_VERSION || cycle.benchmark_id != benchmark_id {
                    bail!(
                        "optimization history {} has incompatible identity",
                        path.display()
                    );
                }
                require_cycle_id(&cycle.cycle_id)?;
                if let Some(previous_cycle_id) = &cycle.previous_cycle_id {
                    require_cycle_id(previous_cycle_id)?;
                }
                if let Some(source_before) = &cycle.source_before {
                    require_hash_id("source state", source_before, "state-")?;
                }
                require_hash_id("source state", &cycle.source_after, "state-")?;
                require_hash_id("source change", &cycle.change_id, "change-")?;
                if let Some(module) = &cycle.benchmark_module {
                    require_portable_file_path(module)?;
                }
                if cycle.previous_cycle_id.as_deref() != last_cycle.as_deref() {
                    bail!(
                        "optimization history {} has a broken cycle chain",
                        path.display()
                    );
                }
                if cycle.source_before.as_deref() != last_source.as_deref() {
                    bail!(
                        "optimization history {} has a broken source chain",
                        path.display()
                    );
                }
                match &cycle.comparison {
                    Some(comparison) => {
                        comparison.validate_contract().with_context(|| {
                            format!(
                                "optimization history {} contains invalid comparison evidence",
                                path.display()
                            )
                        })?;
                        if cycle.baseline_measurement_set.as_deref()
                            != Some(comparison.baseline_measurement_set.as_str())
                            || cycle.candidate_measurement_set
                                != comparison.candidate_measurement_set
                            || comparison.environment_fingerprint.as_deref()
                                != Some(cycle.environment_fingerprint.as_str())
                            || cycle.outcome != comparison.verdict
                        {
                            bail!(
                                "optimization history {} contains an inconsistent cycle comparison",
                                path.display()
                            );
                        }
                    }
                    None => {
                        if cycle.baseline_measurement_set.is_some() || cycle.outcome != "measured" {
                            bail!(
                                "optimization history {} contains an inconsistent measured cycle",
                                path.display()
                            );
                        }
                    }
                }
                let expected_cycle_id = cycle_id(
                    cycle.previous_cycle_id.as_deref(),
                    &cycle.source_after,
                    &cycle.candidate_measurement_set,
                    cycle
                        .comparison
                        .as_ref()
                        .map(|comparison| comparison.comparison_id.as_str()),
                );
                if cycle.cycle_id != expected_cycle_id {
                    bail!(
                        "optimization history {} contains an invalid cycle identity",
                        path.display()
                    );
                }
                if cycles
                    .insert(cycle.cycle_id.clone(), cycle.as_ref().clone())
                    .is_some()
                {
                    bail!(
                        "optimization history {} contains a duplicate cycle",
                        path.display()
                    );
                }
                last_cycle = Some(cycle.cycle_id.clone());
                last_source = Some(cycle.source_after.clone());
            }
            LineageEvent::Confirmation(confirmation) => {
                if confirmation.schema_version != SCHEMA_VERSION
                    || confirmation.benchmark_id != benchmark_id
                {
                    bail!(
                        "optimization history {} has incompatible identity",
                        path.display()
                    );
                }
                require_hash_id(
                    "confirmation",
                    &confirmation.confirmation_id,
                    "confirmation-",
                )?;
                require_cycle_id(&confirmation.cycle_id)?;
                require_hash_id("source state", &confirmation.source_state, "state-")?;
                let cycle = cycles.get(&confirmation.cycle_id).with_context(|| {
                    format!(
                        "optimization history {} confirms an unknown cycle",
                        path.display()
                    )
                })?;
                confirmation
                    .comparison
                    .validate_contract()
                    .with_context(|| {
                        format!(
                            "optimization history {} contains invalid confirmation evidence",
                            path.display()
                        )
                    })?;
                if confirmation.source_state != cycle.source_after
                    || confirmation.original_candidate_measurement_set
                        != cycle.candidate_measurement_set
                    || confirmation.comparison.baseline_measurement_set
                        != cycle
                            .baseline_measurement_set
                            .as_deref()
                            .unwrap_or_default()
                    || confirmation.comparison.candidate_measurement_set
                        != confirmation.confirmation_measurement_set
                    || confirmation.comparison.verdict != confirmation.outcome
                    || confirmation.comparison.environment_fingerprint.as_deref()
                        != Some(&confirmation.environment_fingerprint)
                {
                    bail!(
                        "optimization history {} contains an inconsistent confirmation",
                        path.display()
                    );
                }
                let expected_id = confirmation_id(
                    &confirmation.cycle_id,
                    &confirmation.confirmation_measurement_set,
                    &confirmation.comparison.comparison_id,
                );
                if confirmation.confirmation_id != expected_id
                    || !confirmations.insert(confirmation.confirmation_id.clone())
                {
                    bail!(
                        "optimization history {} contains an invalid confirmation identity",
                        path.display()
                    );
                }
            }
            LineageEvent::Promotion(promotion) => {
                if promotion.schema_version != SCHEMA_VERSION
                    || promotion.benchmark_id != benchmark_id
                {
                    bail!(
                        "optimization history {} has incompatible identity",
                        path.display()
                    );
                }
                require_hash_id("promotion", &promotion.promotion_id, "promotion-")?;
                require_cycle_id(&promotion.cycle_id)?;
                let candidate = cycles.get(&promotion.cycle_id).with_context(|| {
                    format!(
                        "optimization history {} promotes an unknown cycle",
                        path.display()
                    )
                })?;
                if candidate.candidate_measurement_set != promotion.baseline_measurement_set {
                    bail!(
                        "optimization history {} promotes a measurement outside its cycle",
                        path.display()
                    );
                }
                if !promotion_readiness(candidate, &events[..event_index]).ready {
                    bail!(
                        "optimization history {} promotes an ineligible cycle",
                        path.display()
                    );
                }
                let expected_id = promotion_id(
                    &promotion.cycle_id,
                    &promotion.baseline_measurement_set,
                    promotion.previous_baseline_measurement_set.as_deref(),
                );
                if promotion.promotion_id != expected_id
                    || !promotions.insert(promotion.promotion_id.clone())
                {
                    bail!(
                        "optimization history {} contains an invalid promotion identity",
                        path.display()
                    );
                }
            }
        }
    }
    if events.is_empty() {
        bail!("optimization history {} is empty", path.display());
    }
    Ok(events)
}

fn require_promotion_ready(cycle: &CycleRecord, events: &[LineageEvent]) -> Result<()> {
    require_promotable(cycle)?;
    let readiness = promotion_readiness(cycle, events);
    if readiness.ready {
        return Ok(());
    }
    let baseline_measurement_set = cycle
        .baseline_measurement_set
        .as_deref()
        .context("confirmation requirement has no baseline")?;
    bail!(
        "cycle {} needs a fresh confirmation after {} candidates were searched against baseline {}; run `{}`",
        cycle.selector(),
        readiness.searched_candidates,
        baseline_measurement_set,
        cycle.confirm_command()
    );
}

fn require_promotable(cycle: &CycleRecord) -> Result<()> {
    if cycle.promotable() {
        Ok(())
    } else {
        bail!(
            "cycle {} has outcome {} and cannot advance toward promotion; select a measured, positive, or equivalent cycle",
            cycle.selector(),
            cycle.outcome()
        )
    }
}

fn promotion_readiness(cycle: &CycleRecord, events: &[LineageEvent]) -> PromotionReadiness {
    let Some(baseline_measurement_set) = cycle.baseline_measurement_set.as_deref() else {
        return PromotionReadiness {
            confirmation_required: false,
            ready: cycle.promotable(),
            searched_candidates: 0,
            search_threshold: PROMOTION_CONFIRMATION_SEARCHES,
        };
    };
    let searched_candidates = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                LineageEvent::Cycle(candidate)
                    if candidate.baseline_measurement_set.as_deref()
                        == Some(baseline_measurement_set)
            )
        })
        .count();
    if !cycle.promotable() {
        return PromotionReadiness {
            confirmation_required: false,
            ready: false,
            searched_candidates,
            search_threshold: PROMOTION_CONFIRMATION_SEARCHES,
        };
    }
    let confirmation_required = searched_candidates >= PROMOTION_CONFIRMATION_SEARCHES;
    let confirmed = !confirmation_required
        || events.iter().any(|event| {
            matches!(
                event,
                LineageEvent::Confirmation(confirmation)
                    if confirmation.cycle_id == cycle.cycle_id
                        && confirmation_satisfies(&cycle.outcome, &confirmation.outcome)
            )
        });
    PromotionReadiness {
        confirmation_required,
        ready: confirmed,
        searched_candidates,
        search_threshold: PROMOTION_CONFIRMATION_SEARCHES,
    }
}

fn confirmation_satisfies(original: &str, confirmation: &str) -> bool {
    match original {
        "positive" => confirmation == "positive",
        "equivalent" => matches!(confirmation, "positive" | "equivalent"),
        "negative" => confirmation == "negative",
        "inconclusive" => confirmation != "inconclusive",
        "measured" => true,
        _ => false,
    }
}

fn normalize_message(message: Option<String>) -> Result<Option<String>> {
    let message = message.map(|value| value.trim().to_owned());
    let message = message.filter(|value| !value.is_empty());
    if message
        .as_ref()
        .is_some_and(|value| value.len() > MAX_MESSAGE_BYTES)
    {
        bail!("optimization message exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    Ok(message)
}

fn command_path_argument(path: &str) -> String {
    let path = if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_owned()
    };
    if path.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '_' | '-')
    }) {
        return path;
    }

    #[cfg(windows)]
    return format!("'{}'", path.replace('\'', "''"));

    #[cfg(not(windows))]
    format!("'{}'", path.replace('\'', "'\"'\"'"))
}

fn portable_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .context("source path is not valid UTF-8")?
                    .to_owned(),
            ),
            _ => bail!("source path contains a non-relative component"),
        }
    }
    if parts.is_empty() {
        bail!("source file has an empty workspace-relative path");
    }
    Ok(parts.join("/"))
}

fn source_state_id(files: &[SourceFile]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bperf-source-state-v1\0");
    for file in files {
        hash_field(&mut digest, file.path.as_bytes());
        hash_field(&mut digest, file.sha256.as_bytes());
        digest.update(file.size_bytes.to_le_bytes());
    }
    format!("state-{:x}", digest.finalize())
}

fn change_id(before: Option<&str>, after: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bperf-source-change-v1\0");
    hash_field(&mut digest, before.unwrap_or("").as_bytes());
    hash_field(&mut digest, after.as_bytes());
    format!("change-{:x}", digest.finalize())
}

fn cycle_id(
    previous: Option<&str>,
    state: &str,
    measurement: &str,
    comparison: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bperf-cycle-v1\0");
    for field in [
        previous.unwrap_or(""),
        state,
        measurement,
        comparison.unwrap_or(""),
    ] {
        hash_field(&mut digest, field.as_bytes());
    }
    format!("cycle-{:x}", digest.finalize())
}

fn promotion_id(cycle: &str, measurement: &str, previous: Option<&str>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bperf-promotion-v1\0");
    for field in [cycle, measurement, previous.unwrap_or("")] {
        hash_field(&mut digest, field.as_bytes());
    }
    format!("promotion-{:x}", digest.finalize())
}

fn confirmation_id(cycle: &str, measurement: &str, comparison: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bperf-confirmation-v1\0");
    for field in [cycle, measurement, comparison] {
        hash_field(&mut digest, field.as_bytes());
    }
    format!("confirmation-{:x}", digest.finalize())
}

fn hash_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

fn sha256(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn unix_time_ms() -> Result<u64> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(milliseconds).context("system time does not fit in lineage timestamp")
}

fn require_identifier(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        bail!("{label} {value:?} is not a valid bperf identifier");
    }
    Ok(())
}

fn require_cycle_id(value: &str) -> Result<()> {
    require_hash_id("optimization cycle", value, "cycle-")
}

/// Validates a cycle selector's syntax without resolving it against a store.
pub fn validate_cycle_selector(selector: &str) -> Result<()> {
    cycle_selector_prefix(selector).map(|_| ())
}

fn crossover_notice(resolved: &str, benchmarks: &[String]) -> Option<String> {
    let others = benchmarks
        .iter()
        .map(String::as_str)
        .filter(|benchmark| *benchmark != resolved)
        .collect::<Vec<_>>();
    if others.is_empty() {
        return None;
    }
    Some(format!(
        "note: latest resolved to benchmark {resolved:?}; other measured benchmarks: {} \
         (scope with --benchmark)",
        others.join(", ")
    ))
}

fn cycle_selector_prefix(value: &str) -> Result<Option<String>> {
    if value == "latest" {
        return Ok(None);
    }
    let digest = value.strip_prefix("cycle-").unwrap_or(value);
    if !(8..=64).contains(&digest.len()) || !digest.chars().all(|item| item.is_ascii_hexdigit()) {
        bail!(
            "invalid cycle selector {value:?}; use `latest` or at least 8 hexadecimal ID characters"
        );
    }
    Ok(Some(format!("cycle-{}", digest.to_ascii_lowercase())))
}

fn require_hash_id(label: &str, value: &str, prefix: &str) -> Result<()> {
    if !value.strip_prefix(prefix).is_some_and(|digest| {
        digest.len() == 64 && digest.chars().all(|item| item.is_ascii_hexdigit())
    }) {
        bail!("invalid {label} ID {value:?}");
    }
    Ok(())
}

fn require_digest(value: &str) -> Result<()> {
    if value.len() != 64 || !value.chars().all(|item| item.is_ascii_hexdigit()) {
        bail!("invalid source object digest {value:?}");
    }
    Ok(())
}

fn require_portable_file_path(value: &str) -> Result<()> {
    if portable_path(Path::new(value))? != value.replace('\\', "/") || value.contains('\\') {
        bail!("invalid source path {value:?}");
    }
    Ok(())
}

fn render_cycle(cycle: &CycleRecord) -> String {
    let mut output = render_cycle_heading(cycle);
    let _ = writeln!(
        output,
        "  source: {} -> {}",
        cycle.source_before.as_deref().unwrap_or("(initial)"),
        cycle.source_after
    );
    let _ = writeln!(
        output,
        "  measurement set: {}",
        cycle.candidate_measurement_set
    );
    output.push_str(&render_engine_results(cycle));
    output
}

fn render_cycle_heading(cycle: &CycleRecord) -> String {
    let mut output = format!("{}: {}\n", cycle.selector(), cycle.outcome);
    let _ = writeln!(output, "  benchmark: {}", cycle.benchmark_id);
    if let Some(message) = &cycle.message {
        let _ = writeln!(output, "  hypothesis: {message}");
    }
    output
}

/// Renders retained artifact descriptors grouped by engine. Every engine
/// appears even when it retained nothing; paths are listed as recorded and
/// never checked for existence.
fn render_artifacts(artifacts: &[HistoryArtifact]) -> String {
    if artifacts.is_empty() {
        return "  artifacts: (none retained)\n".to_owned();
    }
    let mut output = String::from("  artifacts:\n");
    for engine in Engine::ALL {
        let retained = artifacts
            .iter()
            .filter(|artifact| artifact.engine == Some(engine))
            .collect::<Vec<_>>();
        if retained.is_empty() {
            let _ = writeln!(output, "    {engine}: (none)");
            continue;
        }
        let _ = writeln!(output, "    {engine}:");
        for artifact in retained {
            let _ = writeln!(output, "{}", render_artifact_line(artifact));
        }
    }
    let shared = artifacts
        .iter()
        .filter(|artifact| artifact.engine.is_none())
        .collect::<Vec<_>>();
    if !shared.is_empty() {
        output.push_str("    shared:\n");
        for artifact in shared {
            let _ = writeln!(output, "{}", render_artifact_line(artifact));
        }
    }
    output
}

fn render_artifact_line(artifact: &HistoryArtifact) -> String {
    let mut line = format!("      {}", artifact.kind.label());
    if let Some(scope) = &artifact.capture_scope {
        let _ = write!(line, " {scope}");
    }
    let _ = write!(line, ": {}", artifact.path.display());
    line
}

fn render_engine_results(cycle: &CycleRecord) -> String {
    let Some(comparison) = &cycle.comparison else {
        return "  comparison: no promoted baseline\n".to_owned();
    };
    comparison.render_decision_summary()
}

fn file_diff(
    path: &str,
    before: &[u8],
    after: &[u8],
    before_sha256: Option<&str>,
    after_sha256: Option<&str>,
) -> String {
    let (Ok(before), Ok(after)) = (std::str::from_utf8(before), std::str::from_utf8(after)) else {
        return format!(
            "Binary file {path} changed ({} -> {})\n",
            before_sha256.unwrap_or("(none)"),
            after_sha256.unwrap_or("(none)")
        );
    };
    let old: Vec<_> = before.lines().collect();
    let new: Vec<_> = after.lines().collect();
    let (prefix, old_change_end, new_change_end) = changed_span(&old, &new);
    if prefix == old.len() && prefix == new.len() {
        return format!(
            "--- a/{path}\n+++ b/{path}\n@@ content bytes changed without line changes @@\n"
        );
    }

    let context = 3;
    let start = prefix.saturating_sub(context);
    let old_end = (old_change_end + context).min(old.len());
    let new_end = (new_change_end + context).min(new.len());
    let mut output = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{},{} +{},{} @@\n",
        start + 1,
        old_end - start,
        start + 1,
        new_end - start
    );
    for line in &old[start..prefix] {
        let _ = writeln!(output, " {line}");
    }
    for line in &old[prefix..old_change_end] {
        let _ = writeln!(output, "-{line}");
    }
    for line in &new[prefix..new_change_end] {
        let _ = writeln!(output, "+{line}");
    }
    for line in &old[old_change_end..old_end] {
        let _ = writeln!(output, " {line}");
    }
    output
}

fn changed_span(old: &[&str], new: &[&str]) -> (usize, usize, usize) {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len().saturating_sub(prefix)
        && suffix < new.len().saturating_sub(prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    (prefix, old.len() - suffix, new.len() - suffix)
}

fn short_id(value: &str) -> &str {
    value.get(..value.len().min(20)).unwrap_or(value)
}

#[derive(Serialize)]
struct HistoryReport<'a> {
    schema_version: u32,
    benchmark_id: &'a str,
    events: &'a [LineageEvent],
}

#[derive(Serialize)]
struct ShowReport<'a> {
    schema_version: u32,
    cycle: &'a CycleRecord,
    promotions: &'a [PromotionRecord],
    confirmations: &'a [ConfirmationRecord],
    promotion_readiness: &'a PromotionReadiness,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifacts: Option<&'a [HistoryArtifact]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<SourceChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PromotionReadiness {
    pub confirmation_required: bool,
    pub ready: bool,
    pub searched_candidates: usize,
    pub search_threshold: usize,
}

#[derive(Serialize)]
struct AcceptReport<'a> {
    schema_version: u32,
    status: &'static str,
    cycle: &'a CycleRecord,
    promotion: &'a PromotionRecord,
    baseline: &'a BaselineRecord,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::comparison::{EngineSummary, MetricSummary};

    fn cycle(state: SourceState, measurement: &str) -> NewCycle {
        NewCycle {
            benchmark_id: "parser".to_owned(),
            subject_id: "parser".to_owned(),
            benchmark_sha256: "benchmark".to_owned(),
            candidate_measurement_set: measurement.to_owned(),
            candidate_measurement_path: format!("C:/measurements/{measurement}"),
            environment_fingerprint: "environment".to_owned(),
            source_after: state,
            message: Some(format!("measure {measurement}")),
            comparison: None,
            benchmark_module: None,
        }
    }

    fn comparison(candidate: &str, verdict: &str) -> ComparisonSummary {
        ComparisonSummary {
            comparison_id: format!("compare-{candidate}"),
            report_path: None,
            baseline_measurement_set: "baseline".to_owned(),
            candidate_measurement_set: candidate.to_owned(),
            environment_fingerprint: Some("environment".to_owned()),
            policy: "strict_all".to_owned(),
            verdict: verdict.to_owned(),
            engines: Engine::ALL
                .into_iter()
                .enumerate()
                .map(|(index, engine)| {
                    let baseline = 100.0 + index as f64;
                    let (candidate_value, improvement_pct, ci_pct, classification) = match verdict {
                        "positive" => (baseline * 0.95, 5.0, [3.0, 7.0], "improved"),
                        "negative" => (baseline * 1.05, -5.0, [-7.0, -3.0], "regressed"),
                        "equivalent" => (baseline, 0.0, [-1.0, 1.0], "equivalent"),
                        "inconclusive" => (baseline * 0.98, 2.0, [-1.0, 5.0], "inconclusive"),
                        _ => panic!("unsupported test verdict {verdict:?}"),
                    };
                    EngineSummary {
                        engine,
                        verdict: verdict.to_owned(),
                        correctness: "pass".to_owned(),
                        anchor: Some(crate::comparison::AnchorSummary {
                            status: "stable".to_owned(),
                            drift_pct: Some(0.0),
                            ci_pct: Some([-1.0, 1.0]),
                        }),
                        metrics: BTreeMap::from([(
                            "workload.wall_ms".to_owned(),
                            MetricSummary {
                                improvement_pct: Some(improvement_pct),
                                ci_pct: Some(ci_pct),
                                classification: classification.to_owned(),
                                guardrail_regressed: false,
                                baseline_value: Some(baseline),
                                candidate_value: Some(candidate_value),
                            },
                        )]),
                    }
                })
                .collect(),
            warnings: Vec::new(),
        }
    }

    fn compared_cycle(state: SourceState, measurement: &str) -> NewCycle {
        NewCycle {
            comparison: Some(comparison(measurement, "positive")),
            ..cycle(state, measurement)
        }
    }

    #[test]
    fn history_index_exposes_picker_summaries_without_opening_measurements() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();

        let baseline_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let baseline = store
            .append_cycle(cycle(baseline_state, "measure-1"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        fs::write(&source, "export const value = 2;\n").unwrap();
        let candidate_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let candidate = store
            .append_cycle(compared_cycle(candidate_state, "measure-2"))
            .unwrap();
        let promotion_id = promotion_id(
            &baseline.cycle_id,
            &baseline.candidate_measurement_set,
            None,
        );
        store
            .append_event(
                "parser",
                &LineageEvent::Promotion(PromotionRecord {
                    schema_version: SCHEMA_VERSION,
                    promotion_id,
                    recorded_at_unix_ms: candidate.recorded_at_unix_ms + 1,
                    benchmark_id: "parser".to_owned(),
                    cycle_id: baseline.cycle_id,
                    baseline_measurement_set: baseline.candidate_measurement_set,
                    previous_baseline_measurement_set: None,
                }),
            )
            .unwrap();

        let index = store
            .history_index(&store.database.reader().unwrap())
            .unwrap();
        let entry = &index.benchmarks[0];
        assert_eq!(entry.benchmark_id, "parser");
        assert_eq!(entry.cycle_count, 2);
        assert_eq!(entry.accepted_count, 1);
        assert_eq!(entry.latest_outcome, "positive");
        assert_eq!(entry.latest_message.as_deref(), Some("measure measure-2"));
        assert_eq!(entry.current_baseline_label.as_deref(), Some("b-01"));
        assert_eq!(
            entry
                .latest_comparison
                .as_ref()
                .map(|comparison| comparison.candidate_measurement_set.as_str()),
            Some("measure-2")
        );
        assert_eq!(
            entry.wall_history_ms.get(&Engine::Chromium),
            Some(&vec![95.0])
        );
    }

    #[test]
    fn history_overview_does_not_open_measurements_or_source_objects() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let lineage_root = temporary.path().join("lineages");
        let store = LineageStore::open(&lineage_root).unwrap();

        let baseline_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        store
            .append_cycle(cycle(baseline_state, "measure-1"))
            .unwrap();
        fs::write(&source, "export const value = 2;\n").unwrap();
        let candidate_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let candidate = store
            .append_cycle(compared_cycle(candidate_state, "measure-2"))
            .unwrap();

        for directory in ["objects", "states", "changes"] {
            let path = lineage_root.join(directory);
            if path.is_dir() {
                fs::remove_dir_all(path).unwrap();
            }
        }

        let overview = history_overview(&lineage_root, Some("parser")).unwrap();
        assert_eq!(overview.benchmark_id, "parser");
        assert_eq!(overview.subject_id, "parser");
        assert_eq!(overview.cycles.len(), 2);
        assert_eq!(overview.cycles[0].cycle_id, candidate.cycle_id);
        assert_eq!(overview.cycles[0].outcome, "positive");
        assert_eq!(
            overview.cycles[0]
                .comparison
                .as_ref()
                .map(|comparison| comparison.candidate_measurement_set.as_str()),
            Some("measure-2")
        );
    }

    #[test]
    fn history_cycle_reads_persisted_evidence_without_opening_payload_files() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let lineage_root = temporary.path().join("lineages");
        let store = LineageStore::open(&lineage_root).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let missing_artifact = temporary.path().join("deleted-cpu-profile.json");
        let cycle = store
            .append_cycle_with_evidence(
                cycle(state, "measure-1"),
                Some(NewCycleEvidence {
                    variant_id: "worktree".to_owned(),
                    case_ids: vec!["parse".to_owned()],
                    environment: EnvironmentSummary {
                        recorded_at_unix_ms: 1,
                        fingerprint: "environment".to_owned(),
                        platform: "windows".to_owned(),
                        arch: "x86_64".to_owned(),
                        os_release: "test".to_owned(),
                        browser_versions: Engine::ALL
                            .into_iter()
                            .map(|engine| (engine, "test".to_owned()))
                            .collect(),
                    },
                    artifacts: vec![HistoryArtifact {
                        kind: HistoryArtifactKind::CpuProfile,
                        engine: Some(Engine::Chromium),
                        capture_scope: Some("parse/final/0".to_owned()),
                        path: missing_artifact.clone(),
                    }],
                }),
            )
            .unwrap();
        fs::remove_dir_all(lineage_root.join("objects")).unwrap();

        let detail = history_cycle(&lineage_root, "parser", cycle.cycle_id()).unwrap();
        assert_eq!(detail.variant_id, "worktree");
        assert_eq!(detail.case_ids, ["parse"]);
        assert_eq!(detail.artifacts[0].path, missing_artifact);
    }

    #[test]
    #[ignore = "manual history performance probe"]
    fn history_sqlite_performance_probe() {
        let temporary = tempdir().unwrap();
        let lineage_root = temporary.path().join("lineages");
        let store = LineageStore::open(&lineage_root).unwrap();
        let environment = EnvironmentSummary {
            recorded_at_unix_ms: 1,
            fingerprint: "environment".to_owned(),
            platform: "windows".to_owned(),
            arch: "x86_64".to_owned(),
            os_release: "test".to_owned(),
            browser_versions: Engine::ALL
                .into_iter()
                .map(|engine| (engine, "test".to_owned()))
                .collect(),
        };
        let mut previous_cycle = None;
        let mut previous_state = None;
        let mut selected_cycle = String::new();
        store
            .database
            .write(|transaction| {
                for index in 1_u64..=400 {
                    let cycle_id = format!("cycle-{index:064x}");
                    let source_after = format!("state-{index:064x}");
                    let change_id = format!("change-{index:064x}");
                    let record = CycleRecord {
                        schema_version: SCHEMA_VERSION,
                        cycle_id: cycle_id.clone(),
                        previous_cycle_id: previous_cycle.clone(),
                        recorded_at_unix_ms: index,
                        benchmark_id: "parser".to_owned(),
                        subject_id: "parser".to_owned(),
                        benchmark_sha256: "a".repeat(64),
                        message: Some(format!("candidate {index}")),
                        source_before: previous_state.clone(),
                        source_after: source_after.clone(),
                        change_id,
                        baseline_measurement_set: None,
                        candidate_measurement_set: format!("measure-{index}"),
                        candidate_measurement_path: format!("C:/missing/measure-{index}"),
                        environment_fingerprint: "environment".to_owned(),
                        outcome: "measured".to_owned(),
                        comparison: None,
                        benchmark_module: None,
                    };
                    let evidence = StoredCycleEvidence {
                        schema_version: SCHEMA_VERSION,
                        cycle_id: cycle_id.clone(),
                        variant_id: "worktree".to_owned(),
                        case_ids: vec!["parse".to_owned()],
                        environment: environment.clone(),
                        change: HistoryChangeSummary {
                            files_changed: 1,
                            additions: 1,
                            deletions: 1,
                            binary_files: 0,
                        },
                        artifacts: Vec::new(),
                    };
                    transaction.publish_document(
                        HISTORY_EVIDENCE,
                        &cycle_id,
                        &serde_json::to_vec(&evidence)?,
                    )?;
                    transaction.append_event(
                        LINEAGE_EVENTS,
                        "parser",
                        &serde_json::to_vec(&LineageEvent::Cycle(Box::new(record)))?,
                    )?;
                    previous_cycle = Some(cycle_id.clone());
                    previous_state = Some(source_after);
                    selected_cycle = cycle_id;
                }
                Ok(())
            })
            .unwrap();

        let started = std::time::Instant::now();
        let reader = HistoryReader::open(&lineage_root).unwrap();
        let open_elapsed = started.elapsed();
        let started = std::time::Instant::now();
        let index = reader.index().unwrap();
        let index_elapsed = started.elapsed();
        let started = std::time::Instant::now();
        let overview = reader.overview(Some("parser")).unwrap();
        let overview_elapsed = started.elapsed();
        let selected_summary = overview
            .cycles
            .iter()
            .find(|cycle| cycle.cycle_id == selected_cycle)
            .unwrap();
        let started = std::time::Instant::now();
        let cycle = reader.cycle(selected_summary).unwrap();
        let cycle_elapsed = started.elapsed();

        assert_eq!(index.benchmarks[0].cycle_count, 400);
        assert_eq!(overview.cycles.len(), 400);
        assert_eq!(cycle.cycle_id, selected_cycle);
        eprintln!(
            "400 cycles: open={open_elapsed:?} index={index_elapsed:?} overview={overview_elapsed:?} selected={cycle_elapsed:?}"
        );
    }

    #[test]
    fn checkpoints_preserve_changes_reversions_and_idempotent_retries() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(workspace.join("src")).unwrap();
        let source = workspace.join("src/parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();

        let first_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let first = store.append_cycle(cycle(first_state, "measure-1")).unwrap();

        fs::write(&source, "export const value = 2;\n").unwrap();
        let second_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let second = store
            .append_cycle(cycle(second_state, "measure-2"))
            .unwrap();
        let diff = store.render_change(&second.change_id).unwrap();
        assert!(diff.contains("-export const value = 1;"));
        assert!(diff.contains("+export const value = 2;"));

        fs::write(&source, "export const value = 1;\n").unwrap();
        let reverted_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let reverted = store
            .append_cycle(cycle(reverted_state, "measure-1"))
            .unwrap();
        assert_eq!(reverted.source_after, first.source_after);
        assert_eq!(
            reverted.previous_cycle_id.as_deref(),
            Some(second.cycle_id())
        );

        let retry_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let retry = store.append_cycle(cycle(retry_state, "measure-1")).unwrap();
        assert_eq!(retry.cycle_id(), reverted.cycle_id());
        assert_eq!(
            store.read_events("parser").unwrap().len(),
            3,
            "an exact retry must not create another measured cycle"
        );
    }

    #[test]
    fn human_cycle_selectors_resolve_prefixes_and_the_latest_cycle() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();

        fs::write(&source, "export const value = 1;\n").unwrap();
        let first_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let first = store.append_cycle(cycle(first_state, "measure-1")).unwrap();

        fs::write(&source, "export const value = 2;\n").unwrap();
        let second_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let second = store
            .append_cycle(cycle(second_state, "measure-2"))
            .unwrap();

        let by_prefix = store.find_cycle(second.selector(), None).unwrap().0;
        assert_eq!(by_prefix.cycle_id(), second.cycle_id());
        let bare_prefix = second.selector().strip_prefix("cycle-").unwrap();
        let by_bare_prefix = store.find_cycle(bare_prefix, None).unwrap().0;
        assert_eq!(by_bare_prefix.cycle_id(), second.cycle_id());

        let expected_latest = [&first, &second]
            .into_iter()
            .max_by(|left, right| {
                left.recorded_at_unix_ms
                    .cmp(&right.recorded_at_unix_ms)
                    .then_with(|| left.cycle_id.cmp(&right.cycle_id))
            })
            .unwrap();
        let latest = store.find_cycle("latest", None).unwrap().0;
        assert_eq!(latest.cycle_id(), expected_latest.cycle_id());
        assert_eq!(store.latest_benchmark_id().unwrap(), "parser");

        let error = store.find_cycle("cycle-nope", None).unwrap_err();
        assert!(error.to_string().contains("at least 8 hexadecimal"));
    }

    #[test]
    fn binary_deltas_keep_content_references() {
        let diff = file_diff(
            "fixture.bin",
            &[0, 159, 146, 150],
            &[1, 159, 146, 150],
            Some("before"),
            Some("after"),
        );
        assert!(diff.contains("Binary file fixture.bin changed"));
        assert!(diff.contains("before -> after"));
    }

    #[test]
    fn source_objects_must_match_their_content_address() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let first_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        store.append_cycle(cycle(first_state, "measure-1")).unwrap();

        fs::write(&source, "export const value = 2;\n").unwrap();
        let second_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let second = store
            .append_cycle(cycle(second_state, "measure-2"))
            .unwrap();
        let change = store.load_change(&second.change_id).unwrap();
        let digest = change.files[0].after_sha256.as_deref().unwrap();
        fs::write(store.object_path(digest), "tampered").unwrap();

        let error = store.render_change(&second.change_id).unwrap_err();
        assert!(error.to_string().contains("failed its content digest"));
    }

    #[test]
    fn lineage_history_uses_atomic_database_events() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let first_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        store.append_cycle(cycle(first_state, "measure-1")).unwrap();
        assert_eq!(store.read_events("parser").unwrap().len(), 1);
        assert!(!temporary.path().join("lineages/parser.jsonl").exists());
        fs::write(&source, "export const value = 2;\n").unwrap();
        let second_state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        store
            .append_cycle(cycle(second_state, "measure-2"))
            .unwrap();
        assert_eq!(store.read_events("parser").unwrap().len(), 2);
    }

    #[test]
    fn promotions_must_reference_an_earlier_cycle() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        store.append_cycle(cycle(state, "measure-1")).unwrap();
        store
            .append_event(
                "parser",
                &LineageEvent::Promotion(PromotionRecord {
                    schema_version: SCHEMA_VERSION,
                    promotion_id: format!("promotion-{}", "0".repeat(64)),
                    recorded_at_unix_ms: 1,
                    benchmark_id: "parser".to_owned(),
                    cycle_id: format!("cycle-{}", "f".repeat(64)),
                    baseline_measurement_set: "measure-unknown".to_owned(),
                    previous_baseline_measurement_set: None,
                }),
            )
            .unwrap();

        let error = store.read_events("parser").unwrap_err();
        assert!(error.to_string().contains("promotes an unknown cycle"));
    }

    #[test]
    fn stored_promotions_must_be_eligible_when_recorded() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let negative = store
            .append_cycle(NewCycle {
                comparison: Some(comparison("measure-1", "negative")),
                ..cycle(state, "measure-1")
            })
            .unwrap();
        let promotion_id = promotion_id(
            &negative.cycle_id,
            &negative.candidate_measurement_set,
            None,
        );
        store
            .append_event(
                "parser",
                &LineageEvent::Promotion(PromotionRecord {
                    schema_version: SCHEMA_VERSION,
                    promotion_id,
                    recorded_at_unix_ms: negative.recorded_at_unix_ms + 1,
                    benchmark_id: "parser".to_owned(),
                    cycle_id: negative.cycle_id,
                    baseline_measurement_set: negative.candidate_measurement_set,
                    previous_baseline_measurement_set: None,
                }),
            )
            .unwrap();

        let error = store.read_events("parser").unwrap_err();
        assert!(
            error.to_string().contains("promotes an ineligible cycle"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn repeated_search_requires_an_independent_confirmation() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let mut selected = None;

        for index in 1..=PROMOTION_CONFIRMATION_SEARCHES {
            fs::write(&source, format!("export const value = {index};\n")).unwrap();
            let state = store
                .capture_state(&workspace, std::slice::from_ref(&source))
                .unwrap();
            let cycle = store
                .append_cycle(compared_cycle(state, &format!("measure-{index}")))
                .unwrap();
            let events = store.read_events("parser").unwrap();
            if index < PROMOTION_CONFIRMATION_SEARCHES {
                require_promotion_ready(&cycle, &events).unwrap();
            }
            selected = Some(cycle);
        }

        let selected = selected.unwrap();
        let events = store.read_events("parser").unwrap();
        let error = require_promotion_ready(&selected, &events).unwrap_err();
        assert!(error.to_string().contains("needs a fresh confirmation"));
        let readiness = promotion_readiness(&selected, &events);
        assert!(readiness.confirmation_required);
        assert!(!readiness.ready);
        assert_eq!(
            readiness.searched_candidates,
            PROMOTION_CONFIRMATION_SEARCHES
        );
        assert_eq!(readiness.search_threshold, PROMOTION_CONFIRMATION_SEARCHES);

        store
            .append_confirmation(
                &selected,
                "measure-confirmation",
                Path::new("C:/measurements/confirmation"),
                "environment",
                comparison("measure-confirmation", "positive"),
            )
            .unwrap();
        let events = store.read_events("parser").unwrap();
        require_promotion_ready(&selected, &events).unwrap();
        let readiness = promotion_readiness(&selected, &events);
        assert!(readiness.confirmation_required);
        assert!(readiness.ready);
    }

    fn module_cycle(state: SourceState, measurement: &str) -> NewCycle {
        NewCycle {
            benchmark_module: Some("benchmarks/parser.bench.ts".to_owned()),
            ..cycle(state, measurement)
        }
    }

    fn stream_cycle(benchmark_id: &str, state: SourceState, measurement: &str) -> NewCycle {
        NewCycle {
            benchmark_id: benchmark_id.to_owned(),
            subject_id: benchmark_id.to_owned(),
            ..cycle(state, measurement)
        }
    }

    #[test]
    fn latest_and_prefixes_scope_to_one_benchmark_stream() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let parser_source = workspace.join("parser.ts");
        let encoder_source = workspace.join("encoder.ts");
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();

        fs::write(&parser_source, "export const value = 1;\n").unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&parser_source))
            .unwrap();
        let parser_first = store
            .append_cycle(stream_cycle("parser", state, "measure-parser-1"))
            .unwrap();
        fs::write(&parser_source, "export const value = 2;\n").unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&parser_source))
            .unwrap();
        let parser_second = store
            .append_cycle(stream_cycle("parser", state, "measure-parser-2"))
            .unwrap();
        fs::write(&encoder_source, "export const value = 3;\n").unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&encoder_source))
            .unwrap();
        let encoder = store
            .append_cycle(stream_cycle("encoder", state, "measure-encoder-1"))
            .unwrap();

        let expected_parser_latest = [&parser_first, &parser_second]
            .into_iter()
            .max_by(|left, right| {
                left.recorded_at_unix_ms
                    .cmp(&right.recorded_at_unix_ms)
                    .then_with(|| left.cycle_id.cmp(&right.cycle_id))
            })
            .unwrap();
        let scoped_latest = store.find_cycle("latest", Some("parser")).unwrap().0;
        assert_eq!(scoped_latest.cycle_id(), expected_parser_latest.cycle_id());
        assert_eq!(scoped_latest.benchmark_id, "parser");
        let encoder_latest = store.find_cycle("latest", Some("encoder")).unwrap().0;
        assert_eq!(encoder_latest.cycle_id(), encoder.cycle_id());

        let error = store
            .find_cycle(parser_second.selector(), Some("encoder"))
            .unwrap_err();
        assert!(
            error.to_string().contains("for benchmark \"encoder\""),
            "unexpected error: {error}"
        );
        let error = store.find_cycle("latest", Some("tokenizer")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no optimization history for benchmark"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn crossover_notice_names_other_benchmark_streams() {
        let streams = ["encoder", "parser", "tokenizer"]
            .map(str::to_owned)
            .to_vec();
        let notice = crossover_notice("parser", &streams).unwrap();
        assert!(notice.contains("latest resolved to benchmark \"parser\""));
        assert!(notice.contains("encoder, tokenizer"));
        assert!(notice.contains("--benchmark"));
        assert_eq!(crossover_notice("parser", &["parser".to_owned()]), None);
    }

    #[test]
    fn render_cycle_heading_names_the_benchmark() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let record = store.append_cycle(cycle(state, "measure-1")).unwrap();

        let heading = render_cycle_heading(&record);
        let lines = heading.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], format!("{}: measured", record.selector()));
        assert_eq!(lines[1], "  benchmark: parser");
        assert_eq!(lines[2], "  hypothesis: measure measure-1");
    }

    #[test]
    fn cycle_records_without_benchmark_module_still_load_and_fall_back() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let record = store.append_cycle(cycle(state, "measure-1")).unwrap();

        let serialized = serde_json::to_string(&record).unwrap();
        assert!(
            !serialized.contains("benchmark_module"),
            "an absent module must serialize exactly like a pre-field record"
        );
        let loaded = store.find_cycle(record.selector(), None).unwrap().0;
        assert_eq!(loaded.benchmark_module(), None);
        assert_eq!(
            loaded.confirm_command(),
            format!("bperf confirm <benchmark.bench.ts> {}", loaded.selector())
        );
    }

    #[test]
    fn recorded_benchmark_module_produces_copy_pasteable_hints() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();

        let mut selected = None;
        for index in 1..=PROMOTION_CONFIRMATION_SEARCHES {
            fs::write(&source, format!("export const value = {index};\n")).unwrap();
            let state = store
                .capture_state(&workspace, std::slice::from_ref(&source))
                .unwrap();
            let record = store
                .append_cycle(NewCycle {
                    comparison: Some(comparison(&format!("measure-{index}"), "positive")),
                    ..module_cycle(state, &format!("measure-{index}"))
                })
                .unwrap();
            selected = Some(record);
        }
        let selected = selected.unwrap();
        assert_eq!(
            selected.confirm_command(),
            format!(
                "bperf confirm benchmarks/parser.bench.ts {}",
                selected.selector()
            )
        );

        let events = store.read_events("parser").unwrap();
        let error = require_promotion_ready(&selected, &events).unwrap_err();
        assert!(
            error.to_string().contains(&format!(
                "run `bperf confirm benchmarks/parser.bench.ts {}`",
                selected.selector()
            )),
            "unexpected error: {error}"
        );

        let mut special_path = selected.clone();
        special_path.benchmark_module =
            Some("benchmarks/parser suite's $draft.bench.ts".to_owned());
        #[cfg(not(windows))]
        assert_eq!(
            special_path.confirm_command(),
            format!(
                r#"bperf confirm 'benchmarks/parser suite'"'"'s $draft.bench.ts' {}"#,
                selected.selector()
            )
        );
        #[cfg(windows)]
        assert_eq!(
            special_path.confirm_command(),
            format!(
                "bperf confirm 'benchmarks/parser suite''s $draft.bench.ts' {}",
                selected.selector()
            )
        );

        special_path.benchmark_module = Some("-draft.bench.ts".to_owned());
        assert_eq!(
            special_path.confirm_command(),
            format!("bperf confirm ./-draft.bench.ts {}", selected.selector())
        );
    }

    #[test]
    fn benchmark_module_does_not_change_cycle_identity() {
        let temporary = tempdir().unwrap();
        let mut cycle_ids = Vec::new();
        for (store_name, with_module) in [("with", true), ("without", false)] {
            let workspace = temporary.path().join(format!("workspace-{store_name}"));
            fs::create_dir_all(&workspace).unwrap();
            let source = workspace.join("parser.ts");
            fs::write(&source, "export const value = 1;\n").unwrap();
            let store =
                LineageStore::open(&temporary.path().join(format!("lineages-{store_name}")))
                    .unwrap();
            let state = store
                .capture_state(&workspace, std::slice::from_ref(&source))
                .unwrap();
            let new_cycle = if with_module {
                module_cycle(state, "measure-1")
            } else {
                cycle(state, "measure-1")
            };
            cycle_ids.push(store.append_cycle(new_cycle).unwrap().cycle_id().to_owned());
        }
        assert_eq!(
            cycle_ids[0], cycle_ids[1],
            "the recorded module must not participate in cycle identity"
        );
    }

    #[test]
    fn stored_benchmark_modules_must_be_portable() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let valid = store.append_cycle(cycle(state, "measure-1")).unwrap();

        let mut escaped = valid.clone();
        escaped.benchmark_module = Some("../escape.bench.ts".to_owned());
        escaped.previous_cycle_id = Some(valid.cycle_id().to_owned());
        escaped.source_before = Some(valid.source_after.clone());
        escaped.cycle_id = format!("cycle-{}", "f".repeat(64));
        store
            .append_event("parser", &LineageEvent::Cycle(Box::new(escaped)))
            .unwrap();
        let error = store.read_events("parser").unwrap_err();
        assert!(
            error.to_string().contains("non-relative component"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn stored_cycle_outcome_must_match_its_comparison_evidence() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let negative = store
            .append_cycle(NewCycle {
                comparison: Some(comparison("measure-1", "negative")),
                ..cycle(state, "measure-1")
            })
            .unwrap();

        let mut false_positive = negative.clone();
        false_positive.previous_cycle_id = Some(negative.cycle_id.clone());
        false_positive.source_before = Some(negative.source_after.clone());
        false_positive.change_id = change_id(
            false_positive.source_before.as_deref(),
            &false_positive.source_after,
        );
        false_positive.cycle_id = cycle_id(
            false_positive.previous_cycle_id.as_deref(),
            &false_positive.source_after,
            &false_positive.candidate_measurement_set,
            false_positive
                .comparison
                .as_ref()
                .map(|comparison| comparison.comparison_id.as_str()),
        );
        false_positive.outcome = "positive".to_owned();
        store
            .append_event("parser", &LineageEvent::Cycle(Box::new(false_positive)))
            .unwrap();

        let error = store.read_events("parser").unwrap_err();
        assert!(
            error.to_string().contains("inconsistent cycle comparison"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn stored_cycle_identity_must_match_its_content() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let valid = store.append_cycle(cycle(state, "measure-1")).unwrap();

        let mut copied_identity = valid.clone();
        copied_identity.previous_cycle_id = Some(valid.cycle_id.clone());
        copied_identity.source_before = Some(valid.source_after.clone());
        copied_identity.change_id = change_id(
            copied_identity.source_before.as_deref(),
            &copied_identity.source_after,
        );
        store
            .append_event("parser", &LineageEvent::Cycle(Box::new(copied_identity)))
            .unwrap();

        let error = store.read_events("parser").unwrap_err();
        assert!(
            error.to_string().contains("invalid cycle identity"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn next_command_follows_promotion_readiness() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();

        fs::write(&source, "export const value = 1;\n").unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let promotable = store
            .append_cycle(module_cycle(state, "measure-1"))
            .unwrap();
        fs::write(&source, "export const value = 2;\n").unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let negative = store
            .append_cycle(NewCycle {
                comparison: Some(comparison("measure-2", "negative")),
                ..module_cycle(state, "measure-2")
            })
            .unwrap();

        let ready = PromotionReadiness {
            confirmation_required: false,
            ready: true,
            searched_candidates: 1,
            search_threshold: PROMOTION_CONFIRMATION_SEARCHES,
        };
        let unready = PromotionReadiness {
            confirmation_required: true,
            ready: false,
            searched_candidates: PROMOTION_CONFIRMATION_SEARCHES,
            search_threshold: PROMOTION_CONFIRMATION_SEARCHES,
        };
        assert_eq!(
            promotable.next_command(&ready),
            Some(format!("bperf accept {}", promotable.selector()))
        );
        assert_eq!(
            promotable.next_command(&unready),
            Some(format!(
                "bperf confirm benchmarks/parser.bench.ts {}",
                promotable.selector()
            ))
        );
        assert_eq!(negative.next_command(&ready), None);

        let events = store.read_events("parser").unwrap();
        let negative_readiness = promotion_readiness(&negative, &events);
        assert!(!negative_readiness.ready);
        assert!(!negative_readiness.confirmation_required);
    }

    #[test]
    fn promotion_actions_reject_negative_cycles_before_opening_the_measurement() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let lineage_root = temporary.path().join("lineages");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&lineage_root).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let negative = store
            .append_cycle(NewCycle {
                comparison: Some(comparison("missing-measurement", "negative")),
                ..module_cycle(state, "missing-measurement")
            })
            .unwrap();

        let error = accept(AcceptOptions {
            cycle_id: negative.selector().to_owned(),
            benchmark_id: None,
            root: lineage_root,
            registry_root: temporary.path().join("baselines"),
        })
        .err()
        .expect("a negative cycle must not be accepted");
        assert!(
            error
                .to_string()
                .contains("cannot advance toward promotion"),
            "unexpected error: {error}"
        );
        assert!(!temporary.path().join("baselines").exists());

        let error = confirmation_target(
            &temporary.path().join("lineages"),
            negative.selector(),
            Some("parser"),
        )
        .err()
        .expect("a negative cycle must not be confirmed");
        assert!(
            error
                .to_string()
                .contains("cannot advance toward promotion"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn cycle_evidence_is_optional_for_pre_evidence_cycles() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("parser.ts");
        fs::write(&source, "export const value = 1;\n").unwrap();
        let store = LineageStore::open(&temporary.path().join("lineages")).unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let bare = store.append_cycle(cycle(state, "measure-1")).unwrap();

        let reader = store.database.reader().unwrap();
        assert!(
            store
                .read_cycle_evidence(&reader, bare.cycle_id())
                .unwrap()
                .is_none(),
            "cycles recorded without evidence must not fail show"
        );

        fs::write(&source, "export const value = 2;\n").unwrap();
        let state = store
            .capture_state(&workspace, std::slice::from_ref(&source))
            .unwrap();
        let retained_path = temporary.path().join("cpu.json");
        let with_evidence = store
            .append_cycle_with_evidence(
                cycle(state, "measure-2"),
                Some(NewCycleEvidence {
                    variant_id: "worktree".to_owned(),
                    case_ids: vec!["parse".to_owned()],
                    environment: EnvironmentSummary {
                        recorded_at_unix_ms: 1,
                        fingerprint: "environment".to_owned(),
                        platform: "windows".to_owned(),
                        arch: "x86_64".to_owned(),
                        os_release: "test".to_owned(),
                        browser_versions: Engine::ALL
                            .into_iter()
                            .map(|engine| (engine, "test".to_owned()))
                            .collect(),
                    },
                    artifacts: vec![HistoryArtifact {
                        kind: HistoryArtifactKind::CpuProfile,
                        engine: Some(Engine::Chromium),
                        capture_scope: Some("parse/final/0".to_owned()),
                        path: retained_path.clone(),
                    }],
                }),
            )
            .unwrap();
        let reader = store.database.reader().unwrap();
        let evidence = store
            .read_cycle_evidence(&reader, with_evidence.cycle_id())
            .unwrap()
            .expect("recorded evidence must be readable");
        assert_eq!(evidence.artifacts.len(), 1);
        assert_eq!(evidence.artifacts[0].path, retained_path);
    }

    #[test]
    fn artifact_listing_groups_three_engines_without_pooling() {
        assert_eq!(render_artifacts(&[]), "  artifacts: (none retained)\n");

        let artifacts = vec![
            HistoryArtifact {
                kind: HistoryArtifactKind::CpuProfile,
                engine: Some(Engine::Chromium),
                capture_scope: Some("parse/final/2".to_owned()),
                path: PathBuf::from("/evidence/chromium-cpu.json"),
            },
            HistoryArtifact {
                kind: HistoryArtifactKind::HeapSnapshot,
                engine: Some(Engine::Webkit),
                capture_scope: None,
                path: PathBuf::from("/evidence/webkit-heap.json"),
            },
        ];
        let listing = render_artifacts(&artifacts);
        let lines = listing.lines().collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                "  artifacts:",
                "    chromium:",
                "      cpu_profile parse/final/2: /evidence/chromium-cpu.json",
                "    firefox: (none)",
                "    webkit:",
                "      heap_snapshot: /evidence/webkit-heap.json",
            ]
        );
    }
}
