//! Append-only measured source, confirmation, and baseline-acceptance history.

use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as FmtWrite,
    fs::{self, OpenOptions},
    io::Write as IoWrite,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    baseline::{self, BaselineRecord},
    browser_lab::Engine,
    comparison::ComparisonSummary,
    manifest::VariantDescriptor,
    measurement::MeasurementSet,
};

const SCHEMA_VERSION: u32 = 1;
const MAX_MESSAGE_BYTES: usize = 4096;
const PROMOTION_CONFIRMATION_SEARCHES: usize = 5;

pub(crate) struct RecordRunOptions {
    pub(crate) root: PathBuf,
    pub(crate) workspace_root: PathBuf,
    pub(crate) source_files: Vec<PathBuf>,
    pub(crate) measurement_root: PathBuf,
    pub(crate) message: Option<String>,
    pub(crate) comparison: Option<ComparisonSummary>,
}

pub(crate) fn record_run(options: RecordRunOptions) -> Result<CycleRecord> {
    let measurement = MeasurementSet::open(&options.measurement_root)?;
    let environment_fingerprint = complete_environment(&measurement)?;
    if let Some(comparison) = &options.comparison {
        validate_comparison(&measurement, comparison)?;
    }

    let store = LineageStore::open(&options.root)?;
    let state = capture_measured_state(
        &store,
        &options.workspace_root,
        &options.source_files,
        &measurement,
    )?;
    store.append_cycle(NewCycle {
        benchmark_id: measurement.benchmark_id().to_owned(),
        subject_id: measurement.subject_id().to_owned(),
        benchmark_sha256: measurement.benchmark_sha256().to_owned(),
        candidate_measurement_set: measurement.measurement_set_id().to_owned(),
        candidate_measurement_path: measurement.root().to_string_lossy().into_owned(),
        environment_fingerprint: environment_fingerprint.to_owned(),
        source_after: state,
        message: normalize_message(options.message)?,
        comparison: options.comparison,
    })
}

pub(crate) struct ConfirmationTarget {
    cycle_id: String,
    benchmark_id: String,
    baseline_measurement_set: String,
    candidate_measurement_path: PathBuf,
}

impl ConfirmationTarget {
    pub(crate) fn cycle_id(&self) -> &str {
        &self.cycle_id
    }

    pub(crate) fn benchmark_id(&self) -> &str {
        &self.benchmark_id
    }

    pub(crate) fn baseline_measurement_set(&self) -> &str {
        &self.baseline_measurement_set
    }

    pub(crate) fn candidate_measurement_path(&self) -> &Path {
        &self.candidate_measurement_path
    }
}

pub(crate) fn confirmation_target(root: &Path, cycle_id: &str) -> Result<ConfirmationTarget> {
    require_cycle_id(cycle_id)?;
    let store = LineageStore::load(root)?;
    let (cycle, _, _) = store.find_cycle(cycle_id)?;
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

pub(crate) struct RecordConfirmationOptions {
    pub(crate) root: PathBuf,
    pub(crate) cycle_id: String,
    pub(crate) workspace_root: PathBuf,
    pub(crate) source_files: Vec<PathBuf>,
    pub(crate) measurement_root: PathBuf,
    pub(crate) comparison: ComparisonSummary,
}

pub(crate) fn record_confirmation(
    options: RecordConfirmationOptions,
) -> Result<ConfirmationRecord> {
    require_cycle_id(&options.cycle_id)?;
    let measurement = MeasurementSet::open(&options.measurement_root)?;
    let environment_fingerprint = complete_environment(&measurement)?;
    validate_comparison(&measurement, &options.comparison)?;

    let store = LineageStore::open(&options.root)?;
    let (cycle, _, _) = store.find_cycle(&options.cycle_id)?;
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
    store.append_confirmation(
        &cycle,
        measurement.measurement_set_id(),
        measurement.root(),
        environment_fingerprint,
        options.comparison,
    )
}

fn complete_environment(measurement: &MeasurementSet) -> Result<&str> {
    if !measurement.final_is_complete() {
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
    if comparison.candidate_measurement_set != measurement.measurement_set_id() {
        bail!("comparison candidate does not match the measured source checkpoint");
    }
    if comparison.environment_fingerprint.as_deref() != measurement.environment_fingerprint() {
        bail!("comparison environment does not match the measured source checkpoint");
    }
    let engines: HashSet<_> = comparison
        .engines
        .iter()
        .map(|result| result.engine)
        .collect();
    if comparison.engines.len() != Engine::ALL.len() || engines != HashSet::from(Engine::ALL) {
        bail!("comparison summary does not contain every required browser engine");
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
    let current_variant = VariantDescriptor::load(measurement.variant.source_path())
        .context("failed to verify the measured source state")?;
    if current_variant.source_sha256() != measurement.variant_sha256() {
        bail!(
            "source files changed while measurement set {} was running",
            measurement.measurement_set_id()
        );
    }
    Ok(state)
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum HistoryFormat {
    Text,
    Json,
    AgentContext,
}

pub(crate) struct HistoryOptions {
    pub(crate) benchmark_id: String,
    pub(crate) root: PathBuf,
    pub(crate) format: HistoryFormat,
}

pub(crate) fn history(options: HistoryOptions) -> Result<()> {
    require_identifier("benchmark", &options.benchmark_id)?;
    let store = LineageStore::load(&options.root)?;
    let events = store.read_events(&options.benchmark_id)?;
    match options.format {
        HistoryFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&HistoryReport {
                    schema_version: SCHEMA_VERSION,
                    benchmark_id: &options.benchmark_id,
                    events: &events,
                })?
            );
        }
        HistoryFormat::Text => print!("{}", store.render_history(&options.benchmark_id, &events)?),
        HistoryFormat::AgentContext => {
            print!(
                "{}",
                store.render_agent_context(&options.benchmark_id, &events)?
            );
        }
    }
    Ok(())
}

pub(crate) struct ShowOptions {
    pub(crate) cycle_id: String,
    pub(crate) root: PathBuf,
    pub(crate) diff: bool,
    pub(crate) json: bool,
}

pub(crate) fn show(options: ShowOptions) -> Result<()> {
    require_cycle_id(&options.cycle_id)?;
    let store = LineageStore::load(&options.root)?;
    let (cycle, promotions, confirmations) = store.find_cycle(&options.cycle_id)?;
    let events = store.read_events(&cycle.benchmark_id)?;
    let promotion_readiness = promotion_readiness(&cycle, &events);
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
                change,
                diff,
            })?
        );
    } else {
        print!("{}", render_cycle(&cycle));
        println!(
            "  promotion readiness: {}",
            if promotion_readiness.ready {
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
        if let Some(diff) = diff {
            print!("{diff}");
        }
    }
    Ok(())
}

pub(crate) struct AcceptOptions {
    pub(crate) cycle_id: String,
    pub(crate) root: PathBuf,
    pub(crate) registry_root: PathBuf,
}

pub(crate) fn accept(options: AcceptOptions) -> Result<AcceptOutcome> {
    require_cycle_id(&options.cycle_id)?;
    let store = LineageStore::load(&options.root)?;
    let (cycle, _, _) = store.find_cycle(&options.cycle_id)?;
    let events = store.read_events(&cycle.benchmark_id)?;
    require_promotion_confirmation(&cycle, &events)?;
    let baseline = baseline::promote_measurement(
        Path::new(&cycle.candidate_measurement_path),
        &options.registry_root,
    )?;
    let promotion = store.append_promotion(&cycle, &baseline)?;
    Ok(AcceptOutcome {
        cycle,
        promotion,
        baseline,
    })
}

pub(crate) struct AcceptOutcome {
    cycle: CycleRecord,
    promotion: PromotionRecord,
    baseline: BaselineRecord,
}

impl AcceptOutcome {
    pub(crate) fn report(&self, json: bool) -> Result<()> {
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
            println!("bperf accept: {}", self.cycle.cycle_id);
            println!(
                "  baseline measurement set: {}",
                self.baseline.measurement_set_id()
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CycleRecord {
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
}

impl CycleRecord {
    pub(crate) fn cycle_id(&self) -> &str {
        &self.cycle_id
    }

    pub(crate) fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }

    pub(crate) fn outcome(&self) -> &str {
        &self.outcome
    }
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
pub(crate) struct ConfirmationRecord {
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
    pub(crate) fn confirmation_id(&self) -> &str {
        &self.confirmation_id
    }

    pub(crate) fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }

    pub(crate) fn outcome(&self) -> &str {
        &self.outcome
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
}

struct LineageStore {
    root: PathBuf,
}

impl LineageStore {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("failed to create lineage store {}", root.display()))?;
        for name in ["objects", "states", "changes"] {
            fs::create_dir_all(root.join(name))
                .with_context(|| format!("failed to create lineage {name} directory"))?;
        }
        Self::load(root)
    }

    fn load(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve lineage store {}", root.display()))?;
        Ok(Self { root })
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
        self.write_json(&self.state_path(&state.state_id), &state, "source state")?;
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

    fn append_cycle(&self, cycle: NewCycle) -> Result<CycleRecord> {
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
        };
        self.append_event(
            &record.benchmark_id,
            &LineageEvent::Cycle(Box::new(record.clone())),
        )?;
        Ok(record)
    }

    fn append_promotion(
        &self,
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
        let events = self.read_events(&cycle.benchmark_id)?;
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
        self.append_event(
            &cycle.benchmark_id,
            &LineageEvent::Promotion(record.clone()),
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
        self.append_event(
            &cycle.benchmark_id,
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
        self.write_json(
            &self.change_path(&change.change_id),
            &change,
            "source change",
        )?;
        Ok(change)
    }

    fn read_events(&self, benchmark_id: &str) -> Result<Vec<LineageEvent>> {
        let path = self.history_path(benchmark_id);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("no optimization history for benchmark {benchmark_id:?}"))?;
        parse_events(&path, &source, benchmark_id)
    }

    fn read_events_if_present(&self, benchmark_id: &str) -> Result<Vec<LineageEvent>> {
        let path = self.history_path(benchmark_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        parse_events(&path, &source, benchmark_id)
    }

    fn append_event(&self, benchmark_id: &str, event: &LineageEvent) -> Result<()> {
        let path = self.history_path(benchmark_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open optimization history {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(event)?)
            .with_context(|| format!("failed to append optimization history {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to flush optimization history {}", path.display()))
    }

    fn find_cycle(
        &self,
        cycle_id: &str,
    ) -> Result<(CycleRecord, Vec<PromotionRecord>, Vec<ConfirmationRecord>)> {
        let mut found: Option<(CycleRecord, Vec<PromotionRecord>, Vec<ConfirmationRecord>)> = None;
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("failed to read lineage store {}", self.root.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let benchmark_id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .context("lineage history has a non-UTF-8 name")?;
            let events = self.read_events(benchmark_id)?;
            let cycle = events.iter().find_map(|event| match event {
                LineageEvent::Cycle(record) if record.cycle_id == cycle_id => {
                    Some(record.as_ref().clone())
                }
                _ => None,
            });
            if let Some(cycle) = cycle {
                if found.is_some() {
                    bail!("cycle ID {cycle_id:?} appears in more than one history");
                }
                let promotions = events
                    .iter()
                    .filter_map(|event| match event {
                        LineageEvent::Promotion(record) if record.cycle_id == cycle_id => {
                            Some(record.clone())
                        }
                        _ => None,
                    })
                    .collect();
                let confirmations = events
                    .into_iter()
                    .filter_map(|event| match event {
                        LineageEvent::Confirmation(record) if record.cycle_id == cycle_id => {
                            Some(record.as_ref().clone())
                        }
                        _ => None,
                    })
                    .collect();
                found = Some((cycle, promotions, confirmations));
            }
        }
        found.with_context(|| format!("no optimization cycle {cycle_id:?}"))
    }

    fn render_history(&self, benchmark_id: &str, events: &[LineageEvent]) -> Result<String> {
        let mut output = format!("bperf history: {benchmark_id}\n");
        for event in events {
            match event {
                LineageEvent::Cycle(cycle) => {
                    output.push_str(&render_cycle(cycle));
                    let readiness = promotion_readiness(cycle, events);
                    if readiness.confirmation_required {
                        writeln!(
                            output,
                            "  promotion confirmation: {} ({}/{} searched candidates)",
                            if readiness.ready {
                                "satisfied"
                            } else {
                                "required"
                            },
                            readiness.searched_candidates,
                            readiness.search_threshold
                        )?;
                    }
                }
                LineageEvent::Confirmation(confirmation) => {
                    writeln!(
                        output,
                        "confirmation {}: {} -> {}",
                        short_id(&confirmation.confirmation_id),
                        short_id(&confirmation.cycle_id),
                        confirmation.outcome
                    )?;
                }
                LineageEvent::Promotion(promotion) => {
                    writeln!(
                        output,
                        "promotion {}: {} -> baseline",
                        short_id(&promotion.promotion_id),
                        short_id(&promotion.cycle_id)
                    )?;
                }
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
        let state: SourceState =
            self.read_json(&self.state_path(state_id), state_id, "source state")?;
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
        let change: SourceChange =
            self.read_json(&self.change_path(change_id), change_id, "source change")?;
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

    fn read_json<Value: for<'de> Deserialize<'de>>(
        &self,
        path: &Path,
        id: &str,
        label: &str,
    ) -> Result<Value> {
        serde_json::from_slice(
            &fs::read(path).with_context(|| format!("failed to read {label} {id}"))?,
        )
        .with_context(|| format!("invalid {label} {}", path.display()))
    }

    fn write_json<Value: Serialize>(&self, path: &Path, value: &Value, label: &str) -> Result<()> {
        self.write_immutable(
            path,
            format!("{}\n", serde_json::to_string_pretty(value)?).as_bytes(),
        )
        .with_context(|| format!("failed to store {label} {}", path.display()))
    }

    fn write_immutable(&self, path: &Path, content: &[u8]) -> Result<()> {
        if path.exists() {
            let existing =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            if existing == content {
                return Ok(());
            }
            bail!("lineage object collision at {}", path.display());
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

    fn history_path(&self, benchmark_id: &str) -> PathBuf {
        self.root.join(format!("{benchmark_id}.jsonl"))
    }

    fn object_path(&self, sha256: &str) -> PathBuf {
        self.root.join("objects").join(sha256)
    }

    fn state_path(&self, state_id: &str) -> PathBuf {
        self.root.join("states").join(format!("{state_id}.json"))
    }

    fn change_path(&self, change_id: &str) -> PathBuf {
        self.root.join("changes").join(format!("{change_id}.json"))
    }
}

fn parse_events(path: &Path, source: &str, benchmark_id: &str) -> Result<Vec<LineageEvent>> {
    let mut events = Vec::new();
    let mut last_cycle: Option<String> = None;
    let mut last_source: Option<String> = None;
    let mut cycles = BTreeMap::new();
    let mut confirmations = HashSet::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: LineageEvent = serde_json::from_str(line)
            .with_context(|| format!("invalid {} line {}", path.display(), index + 1))?;
        match &event {
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
            }
        }
        events.push(event);
    }
    if events.is_empty() {
        bail!("optimization history {} is empty", path.display());
    }
    Ok(events)
}

fn require_promotion_confirmation(cycle: &CycleRecord, events: &[LineageEvent]) -> Result<()> {
    let readiness = promotion_readiness(cycle, events);
    if readiness.ready {
        return Ok(());
    }
    let baseline_measurement_set = cycle
        .baseline_measurement_set
        .as_deref()
        .context("confirmation requirement has no baseline")?;
    bail!(
        "cycle {} needs a fresh confirmation after {} candidates were searched against baseline {}; run `bperf confirm {} <benchmark.ts>`",
        cycle.cycle_id,
        readiness.searched_candidates,
        baseline_measurement_set,
        cycle.cycle_id
    );
}

fn promotion_readiness(cycle: &CycleRecord, events: &[LineageEvent]) -> PromotionReadiness {
    let Some(baseline_measurement_set) = cycle.baseline_measurement_set.as_deref() else {
        return PromotionReadiness {
            confirmation_required: false,
            ready: true,
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
    let mut output = format!("{}: {}\n", cycle.cycle_id, cycle.outcome);
    if let Some(message) = &cycle.message {
        let _ = writeln!(output, "  hypothesis: {message}");
    }
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
    if prefix == old.len() && prefix == new.len() {
        return format!(
            "--- a/{path}\n+++ b/{path}\n@@ content bytes changed without line changes @@\n"
        );
    }

    let context = 3;
    let old_change_end = old.len() - suffix;
    let new_change_end = new.len() - suffix;
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
    change: Option<SourceChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
}

#[derive(Serialize)]
struct PromotionReadiness {
    confirmation_required: bool,
    ready: bool,
    searched_candidates: usize,
    search_threshold: usize,
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
    use crate::comparison::EngineSummary;

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
        }
    }

    fn comparison(candidate: &str, verdict: &str) -> ComparisonSummary {
        ComparisonSummary {
            comparison_id: format!("compare-{candidate}"),
            report_path: format!("C:/comparisons/{candidate}.json"),
            baseline_measurement_set: "baseline".to_owned(),
            candidate_measurement_set: candidate.to_owned(),
            environment_fingerprint: Some("environment".to_owned()),
            policy: "strict_all".to_owned(),
            verdict: verdict.to_owned(),
            engines: Engine::ALL
                .into_iter()
                .map(|engine| EngineSummary {
                    engine,
                    verdict: verdict.to_owned(),
                    correctness: "pass".to_owned(),
                    anchor: None,
                    metrics: BTreeMap::new(),
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
                require_promotion_confirmation(&cycle, &events).unwrap();
            }
            selected = Some(cycle);
        }

        let selected = selected.unwrap();
        let events = store.read_events("parser").unwrap();
        let error = require_promotion_confirmation(&selected, &events).unwrap_err();
        assert!(error.to_string().contains("needs a fresh confirmation"));

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
        require_promotion_confirmation(&selected, &events).unwrap();
    }
}
