//! Benchmark and variant definition parsing.
//!
//! A benchmark fixes comparable work and policy. A variant only describes one
//! implementation and its invocation adapter.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::browser_lab::{BrowserTrialConfig, Engine, Viewport};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkManifest {
    schema_version: u32,
    benchmark: BenchmarkIdentity,
    workloads: Vec<WorkloadSpec>,
    browser: BrowserSpec,
    captures: CapturesSpec,
    trials: TrialsSpec,
    statistics: StatisticsSpec,
    #[serde(skip)]
    source_path: PathBuf,
    #[serde(skip)]
    source_sha256: String,
}

impl BenchmarkManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let source_path = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve benchmark {}", path.display()))?;
        let source = fs::read(&source_path)
            .with_context(|| format!("failed to read benchmark {}", source_path.display()))?;
        let mut manifest: Self = serde_saphyr::from_slice(&source)
            .with_context(|| format!("invalid benchmark YAML in {}", source_path.display()))?;
        manifest.source_path = source_path;
        manifest.source_sha256 = String::new();
        manifest.resolve_paths();
        manifest.validate()?;
        manifest.source_sha256 = definition_sha256(
            b"bperf-benchmark-definition-v1\0",
            &source,
            manifest.identity_files(),
        )?;
        Ok(manifest)
    }

    pub fn load_resolved(path: &Path) -> Result<Self> {
        let resolved_path = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        let source = fs::read(&resolved_path)
            .with_context(|| format!("failed to read {}", resolved_path.display()))?;
        let mut value: serde_json::Value = serde_json::from_slice(&source)
            .with_context(|| format!("invalid resolved benchmark {}", resolved_path.display()))?;
        let source_metadata = value
            .as_object_mut()
            .and_then(|object| object.remove("_source"))
            .context("resolved benchmark has no _source metadata")?;
        let metadata: ResolvedSourceOwned = serde_json::from_value(source_metadata)
            .context("resolved benchmark has invalid _source metadata")?;
        let mut manifest: Self =
            serde_json::from_value(value).context("invalid resolved benchmark fields")?;
        manifest.source_path = PathBuf::from(metadata.path);
        manifest.source_sha256 = metadata.sha256;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn benchmark_id(&self) -> &str {
        &self.benchmark.id
    }

    pub fn subject_id(&self) -> &str {
        &self.benchmark.subject
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }

    pub fn workload_ids(&self) -> impl Iterator<Item = &str> {
        self.workloads.iter().map(|workload| workload.id.as_str())
    }

    pub fn workload(&self, id: &str) -> Option<WorkloadInvocation<'_>> {
        self.workloads
            .iter()
            .find(|workload| workload.id == id)
            .map(|workload| WorkloadInvocation {
                trace_file: &workload.trace_file,
                verifier: workload
                    .verifier
                    .invocation(self.source_path.parent().unwrap_or_else(|| Path::new("."))),
            })
    }

    pub fn engines(&self) -> &[Engine] {
        &self.browser.engines
    }

    pub fn browser_trial_config(&self) -> BrowserTrialConfig {
        BrowserTrialConfig {
            viewport: Viewport {
                width: self.browser.viewport.width,
                height: self.browser.viewport.height,
            },
            locale: self.browser.locale.clone(),
            timezone_id: self.browser.timezone.clone(),
            color_scheme: self.browser.color_scheme.clone(),
        }
    }

    pub fn schedule_seed(&self) -> u64 {
        self.trials.schedule_seed
    }

    pub fn randomize_order(&self) -> bool {
        self.trials.randomize_order
    }

    pub fn warmup_samples(&self) -> u32 {
        self.trials.warmup_samples
    }

    pub fn pilot_samples(&self) -> u32 {
        self.trials.pilot_samples
    }

    pub(crate) fn adaptive_final_sample_range(&self) -> Result<(u32, u32)> {
        match &self.trials.mode {
            TrialCount::Label(label) if label == "auto" => {}
            TrialCount::Label(label) => {
                bail!("unsupported trials.mode {label:?}; expected `auto` or an integer")
            }
            TrialCount::Count(_) => {
                bail!("adaptive sampling requires trials.mode `auto`");
            }
        }
        if self.trials.pilot_samples < 2 {
            bail!("adaptive sampling requires at least two pilot samples");
        }
        Ok((self.trials.min_final_samples, self.trials.max_final_samples))
    }

    pub fn resolve_final_samples(&self, requested: Option<u32>) -> Result<u32> {
        let fixed = match &self.trials.mode {
            TrialCount::Label(label) if label == "auto" => None,
            TrialCount::Label(label) => {
                bail!("unsupported trials.mode {label:?}; expected `auto` or an integer")
            }
            TrialCount::Count(count) => Some(*count),
        };

        let final_samples = match (fixed, requested) {
            (Some(fixed), Some(requested)) if fixed != requested => {
                bail!(
                    "benchmark fixes {fixed} final samples but --final-samples requested {requested}"
                )
            }
            (Some(fixed), _) => fixed,
            (None, Some(requested)) => requested,
            (None, None) => bail!(
                "trials.mode is `auto`; provide --final-samples after the pilot locks the sample size"
            ),
        };
        if !(self.trials.min_final_samples..=self.trials.max_final_samples).contains(&final_samples)
        {
            bail!(
                "final sample count {final_samples} is outside the declared range {}..={}",
                self.trials.min_final_samples,
                self.trials.max_final_samples
            );
        }
        Ok(final_samples)
    }

    pub fn validate_variant(&self, variant: &VariantDescriptor) -> Result<()> {
        if variant.subject_id() != self.subject_id() {
            bail!(
                "variant subject {:?} does not match benchmark subject {:?}",
                variant.subject_id(),
                self.subject_id()
            );
        }
        Ok(())
    }

    pub fn resolved_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&ResolvedBenchmark {
            source: ResolvedSource {
                path: self.source_path.to_string_lossy(),
                sha256: &self.source_sha256,
            },
            manifest: self,
        })
        .context("failed to encode resolved benchmark")
    }

    pub fn analysis_policy(&self) -> AnalysisPolicy {
        AnalysisPolicy {
            confidence: self.statistics.confidence,
            bootstrap_samples: self.statistics.bootstrap_samples,
            primary_metrics: self
                .statistics
                .primary_metrics
                .iter()
                .map(|metric| MetricPolicy {
                    name: metric.clone(),
                    minimum_effect_pct: self.statistics.minimum_effect_pct[metric],
                })
                .collect(),
            minimum_success_rate: self.statistics.correctness.minimum_success_rate,
            max_regression_percentage_points: self
                .statistics
                .correctness
                .max_regression_percentage_points,
            protected_metric_max_regression_pct: self
                .statistics
                .protected_metric_max_regression_pct,
        }
    }

    fn resolve_paths(&mut self) {
        let base = self.source_path.parent().unwrap_or_else(|| Path::new("."));
        for workload in &mut self.workloads {
            workload.trace_file = resolve(base, &workload.trace_file);
            for file in &mut workload.identity_files {
                *file = resolve(base, file);
            }
            workload.verifier.resolve_paths(base);
        }
    }

    fn identity_files(&self) -> impl Iterator<Item = &Path> {
        self.workloads.iter().flat_map(|workload| {
            std::iter::once(workload.trace_file.as_path())
                .chain(workload.identity_files.iter().map(PathBuf::as_path))
                .chain(workload.verifier.identity_files().map(PathBuf::as_path))
        })
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported benchmark schema {}; expected {}",
                self.schema_version,
                SCHEMA_VERSION
            );
        }
        require_identifier("benchmark id", &self.benchmark.id)?;
        require_identifier("benchmark subject", &self.benchmark.subject)?;
        if self.workloads.is_empty() {
            bail!("benchmark must define at least one workload");
        }
        let mut workload_ids = HashSet::new();
        for workload in &self.workloads {
            require_identifier("workload id", &workload.id)?;
            if !workload_ids.insert(workload.id.as_str()) {
                bail!("duplicate workload id {:?}", workload.id);
            }
            require_unique_paths("workload identity_files", &workload.identity_files)?;
            workload.verifier.validate()?;
        }

        if self.browser.engines.is_empty() {
            bail!("browser.engines must select at least one engine");
        }
        let engine_set: HashSet<_> = self.browser.engines.iter().copied().collect();
        if engine_set.len() != self.browser.engines.len() {
            bail!("browser.engines contains a duplicate engine");
        }
        require_nonempty("browser mode", &self.browser.mode)?;
        require_nonempty("browser locale", &self.browser.locale)?;
        require_nonempty("browser timezone", &self.browser.timezone)?;
        require_nonempty("browser color_scheme", &self.browser.color_scheme)?;
        require_nonempty("browser cache policy", &self.browser.cache)?;
        require_nonempty("browser network policy", &self.browser.network.policy)?;
        require_positive("browser viewport width", self.browser.viewport.width)?;
        require_positive("browser viewport height", self.browser.viewport.height)?;
        if self.browser.mode != "headless" {
            bail!("version 1 supports only browser.mode `headless`");
        }
        if !["light", "dark", "no-preference"].contains(&self.browser.color_scheme.as_str()) {
            bail!("browser.color_scheme must be light, dark, or no-preference");
        }
        if self.browser.cache != "cold" {
            bail!("version 1 supports only browser.cache `cold`");
        }
        if self.browser.network.policy != "local-only" {
            bail!("version 1 supports only browser.network.policy `local-only`");
        }
        if !self.browser.fresh_profile_per_trial {
            bail!("browser.fresh_profile_per_trial must be true");
        }

        if !self.captures.cpu_profile || !self.captures.js_heap {
            bail!("version 1 requires browser CPU profiles and JavaScript heaps");
        }
        require_positive("trials.min_final_samples", self.trials.min_final_samples)?;
        if self.trials.max_final_samples < self.trials.min_final_samples {
            bail!("trials.max_final_samples must be >= trials.min_final_samples");
        }
        if let TrialCount::Count(count) = self.trials.mode {
            require_positive("trials.mode", count)?;
            if !(self.trials.min_final_samples..=self.trials.max_final_samples).contains(&count) {
                bail!("fixed trials.mode must be within min_final_samples..=max_final_samples");
            }
        }

        if !(0.0..1.0).contains(&self.statistics.confidence) {
            bail!("statistics.confidence must be between 0 and 1");
        }
        if self.statistics.bootstrap_samples < 1_000 {
            bail!("statistics.bootstrap_samples must be at least 1000");
        }
        if self.statistics.primary_metrics.is_empty() {
            bail!("statistics.primary_metrics must not be empty");
        }
        for metric in &self.statistics.primary_metrics {
            require_nonempty("primary metric", metric)?;
            if ![
                "workload.wall_ms",
                "variant.call_wall_ms",
                "browser.cpu_profile.active_ms",
                "browser.js_heap.live_bytes",
            ]
            .contains(&metric.as_str())
            {
                bail!("primary metric {metric:?} is not produced by a browser trial");
            }
            let effect = self
                .statistics
                .minimum_effect_pct
                .get(metric)
                .with_context(|| format!("primary metric {metric:?} has no minimum_effect_pct"))?;
            if !effect.is_finite() || *effect < 0.0 {
                bail!("minimum effect for {metric:?} must be finite and non-negative");
            }
        }
        if !(0.0..=1.0).contains(&self.statistics.correctness.minimum_success_rate) {
            bail!("correctness.minimum_success_rate must be between 0 and 1");
        }
        if !(0.0..=100.0).contains(&self.statistics.correctness.max_regression_percentage_points) {
            bail!("correctness.max_regression_percentage_points must be between 0 and 100");
        }
        if self.statistics.cross_engine_policy != "strict_all" {
            bail!("version 1 supports only statistics.cross_engine_policy `strict_all`");
        }
        if !self
            .statistics
            .protected_metric_max_regression_pct
            .is_finite()
            || self.statistics.protected_metric_max_regression_pct < 0.0
        {
            bail!("protected_metric_max_regression_pct must be finite and non-negative");
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VariantDescriptor {
    schema_version: u32,
    id: String,
    subject: String,
    adapter: VariantAdapterSpec,
    implementation: ImplementationSpec,
    #[serde(skip)]
    source_path: PathBuf,
    #[serde(skip)]
    source_sha256: String,
}

impl VariantDescriptor {
    pub fn load(path: &Path) -> Result<Self> {
        let source_path = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve variant {}", path.display()))?;
        let source = fs::read(&source_path)
            .with_context(|| format!("failed to read variant {}", source_path.display()))?;
        let mut variant: Self = serde_saphyr::from_slice(&source)
            .with_context(|| format!("invalid variant YAML in {}", source_path.display()))?;
        variant.source_path = source_path;
        variant.source_sha256 = String::new();
        variant.resolve_paths();
        variant.validate()?;
        variant.source_sha256 = definition_sha256(
            b"bperf-variant-definition-v1\0",
            &source,
            variant.implementation.files.iter().map(PathBuf::as_path),
        )?;
        Ok(variant)
    }

    pub fn load_resolved(path: &Path) -> Result<Self> {
        let resolved_path = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve {}", path.display()))?;
        let source = fs::read(&resolved_path)
            .with_context(|| format!("failed to read {}", resolved_path.display()))?;
        let mut value: serde_json::Value = serde_json::from_slice(&source)
            .with_context(|| format!("invalid resolved variant {}", resolved_path.display()))?;
        let source_metadata = value
            .as_object_mut()
            .and_then(|object| object.remove("_source"))
            .context("resolved variant has no _source metadata")?;
        let metadata: ResolvedSourceOwned = serde_json::from_value(source_metadata)
            .context("resolved variant has invalid _source metadata")?;
        let mut variant: Self =
            serde_json::from_value(value).context("invalid resolved variant fields")?;
        variant.source_path = PathBuf::from(metadata.path);
        variant.source_sha256 = metadata.sha256;
        variant.validate()?;
        Ok(variant)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn subject_id(&self) -> &str {
        &self.subject
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }

    pub fn invocation(&self) -> VariantInvocation<'_> {
        VariantInvocation {
            command: &self.adapter.command,
            ready_timeout: Duration::from_secs(self.adapter.ready.timeout_seconds),
            working_directory: self.source_path.parent().unwrap_or_else(|| Path::new(".")),
        }
    }

    pub fn resolved_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&ResolvedVariant {
            source: ResolvedSource {
                path: self.source_path.to_string_lossy(),
                sha256: &self.source_sha256,
            },
            variant: self,
        })
        .context("failed to encode resolved variant")
    }

    fn resolve_paths(&mut self) {
        let base = self.source_path.parent().unwrap_or_else(|| Path::new("."));
        for file in &mut self.implementation.files {
            *file = resolve(base, file);
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported variant schema {}; expected {}",
                self.schema_version,
                SCHEMA_VERSION
            );
        }
        require_identifier("variant id", &self.id)?;
        require_identifier("variant subject", &self.subject)?;
        validate_command("variant adapter", &self.adapter.command)?;
        if self.implementation.files.is_empty() {
            bail!("variant implementation.files must not be empty");
        }
        require_unique_paths("variant implementation.files", &self.implementation.files)?;
        require_nonempty("variant ready protocol", &self.adapter.ready.protocol)?;
        if self.adapter.ready.protocol != "stdio-json" {
            bail!("version 1 supports only variant ready protocol `stdio-json`");
        }
        require_positive(
            "variant ready timeout_seconds",
            self.adapter.ready.timeout_seconds,
        )
    }
}

#[cfg(test)]
fn sha256(source: &[u8]) -> String {
    format!("{:x}", Sha256::digest(source))
}

fn definition_sha256<'a>(
    domain: &[u8],
    definition: &[u8],
    files: impl Iterator<Item = &'a Path>,
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update((definition.len() as u64).to_le_bytes());
    digest.update(definition);
    for file in files {
        let content = fs::read(file)
            .with_context(|| format!("failed to read identity file {}", file.display()))?;
        digest.update((content.len() as u64).to_le_bytes());
        digest.update(content);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn resolve(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_owned()
    } else {
        base.join(value)
    }
}

fn require_identifier(label: &str, value: &str) -> Result<()> {
    require_nonempty(label, value)?;
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        bail!("{label} {value:?} may contain only ASCII letters, numbers, `-`, `_`, and `.`");
    }
    Ok(())
}

fn require_nonempty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(())
}

fn require_positive<T>(label: &str, value: T) -> Result<()>
where
    T: PartialEq + From<u8> + std::fmt::Display,
{
    if value == T::from(0) {
        bail!("{label} must be positive");
    }
    Ok(())
}

fn validate_command(label: &str, command: &[String]) -> Result<()> {
    if command.is_empty() || command.iter().any(|part| part.is_empty()) {
        bail!("{label} command must contain only non-empty arguments");
    }
    Ok(())
}

fn require_unique_paths(label: &str, paths: &[PathBuf]) -> Result<()> {
    let unique: HashSet<_> = paths.iter().collect();
    if unique.len() != paths.len() {
        bail!("{label} contains a duplicate path");
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BenchmarkIdentity {
    id: String,
    subject: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkloadSpec {
    id: String,
    trace_file: PathBuf,
    #[serde(default)]
    identity_files: Vec<PathBuf>,
    verifier: VerifierSpec,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum VerifierSpec {
    Process(CommandWithTimeout),
    BuiltIn(BuiltInVerifierSpec),
}

impl VerifierSpec {
    fn resolve_paths(&mut self, base: &Path) {
        if let Self::Process(process) = self {
            for file in &mut process.identity_files {
                *file = resolve(base, file);
            }
        }
    }

    fn identity_files(&self) -> impl Iterator<Item = &PathBuf> {
        match self {
            Self::Process(process) => process.identity_files.iter(),
            Self::BuiltIn(_) => [].iter(),
        }
    }

    fn validate(&self) -> Result<()> {
        if let Self::Process(process) = self {
            validate_command("workload verifier", &process.command)?;
            if process.identity_files.is_empty() {
                bail!("workload verifier identity_files must not be empty");
            }
            require_unique_paths("workload verifier identity_files", &process.identity_files)?;
            require_positive("workload verifier timeout_seconds", process.timeout_seconds)?;
        }
        Ok(())
    }

    fn invocation<'a>(&'a self, working_directory: &'a Path) -> VerifierInvocation<'a> {
        match self {
            Self::Process(process) => VerifierInvocation::Process {
                command: &process.command,
                timeout: Duration::from_secs(process.timeout_seconds),
                working_directory,
            },
            Self::BuiltIn(BuiltInVerifierSpec {
                builtin: BuiltInVerifier::Exact,
            }) => VerifierInvocation::Exact,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CommandWithTimeout {
    command: Vec<String>,
    timeout_seconds: u64,
    identity_files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BuiltInVerifierSpec {
    builtin: BuiltInVerifier,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BuiltInVerifier {
    Exact,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VariantAdapterSpec {
    command: Vec<String>,
    ready: ReadySpec,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImplementationSpec {
    files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadySpec {
    protocol: String,
    timeout_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BrowserSpec {
    engines: Vec<Engine>,
    mode: String,
    viewport: ViewportSpec,
    locale: String,
    timezone: String,
    color_scheme: String,
    cache: String,
    network: NetworkSpec,
    fresh_profile_per_trial: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ViewportSpec {
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NetworkSpec {
    policy: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CapturesSpec {
    cpu_profile: bool,
    js_heap: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrialsSpec {
    mode: TrialCount,
    randomize_order: bool,
    schedule_seed: u64,
    warmup_samples: u32,
    pilot_samples: u32,
    min_final_samples: u32,
    max_final_samples: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum TrialCount {
    Label(String),
    Count(u32),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StatisticsSpec {
    confidence: f64,
    bootstrap_samples: u32,
    primary_metrics: Vec<String>,
    minimum_effect_pct: BTreeMap<String, f64>,
    correctness: CorrectnessSpec,
    cross_engine_policy: String,
    protected_metric_max_regression_pct: f64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CorrectnessSpec {
    minimum_success_rate: f64,
    max_regression_percentage_points: f64,
}

#[derive(Serialize)]
struct ResolvedBenchmark<'a> {
    #[serde(rename = "_source")]
    source: ResolvedSource<'a>,
    #[serde(flatten)]
    manifest: &'a BenchmarkManifest,
}

#[derive(Serialize)]
struct ResolvedVariant<'a> {
    #[serde(rename = "_source")]
    source: ResolvedSource<'a>,
    #[serde(flatten)]
    variant: &'a VariantDescriptor,
}

#[derive(Serialize)]
struct ResolvedSource<'a> {
    path: std::borrow::Cow<'a, str>,
    sha256: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedSourceOwned {
    path: String,
    sha256: String,
}

pub struct WorkloadInvocation<'a> {
    pub trace_file: &'a Path,
    pub verifier: VerifierInvocation<'a>,
}

pub enum VerifierInvocation<'a> {
    Exact,
    Process {
        command: &'a [String],
        timeout: Duration,
        working_directory: &'a Path,
    },
}

pub struct VariantInvocation<'a> {
    pub command: &'a [String],
    pub ready_timeout: Duration,
    pub working_directory: &'a Path,
}

pub struct AnalysisPolicy {
    pub confidence: f64,
    pub bootstrap_samples: u32,
    pub primary_metrics: Vec<MetricPolicy>,
    pub minimum_success_rate: f64,
    pub max_regression_percentage_points: f64,
    pub protected_metric_max_regression_pct: f64,
}

pub struct MetricPolicy {
    pub name: String,
    pub minimum_effect_pct: f64,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    const BENCHMARK: &str = include_str!("../examples/browser-benchmark.yaml");
    const VARIANT: &str = include_str!("../examples/browser-variant-baseline.yaml");

    fn parse_benchmark(source: &str) -> Result<BenchmarkManifest> {
        let mut manifest: BenchmarkManifest = serde_saphyr::from_str(source)?;
        manifest.source_path = PathBuf::from("C:/fixture/benchmark.yaml");
        manifest.source_sha256 = sha256(source.as_bytes());
        manifest.resolve_paths();
        manifest.validate()?;
        Ok(manifest)
    }

    fn parse_variant(source: &str) -> Result<VariantDescriptor> {
        let mut variant: VariantDescriptor = serde_saphyr::from_str(source)?;
        variant.source_path = PathBuf::from("C:/fixture/variant.yaml");
        variant.source_sha256 = sha256(source.as_bytes());
        variant.validate()?;
        Ok(variant)
    }

    #[test]
    fn accepts_compatible_benchmark_and_variant_definitions() {
        let benchmark = parse_benchmark(BENCHMARK).unwrap();
        let variant = parse_variant(VARIANT).unwrap();
        benchmark.validate_variant(&variant).unwrap();
        assert_eq!(benchmark.engines(), Engine::ALL);
        assert_eq!(benchmark.benchmark_id(), "browser-operation-benchmark");
        assert_eq!(benchmark.subject_id(), "browser-operation-adapter");
        assert_eq!(variant.id(), "browser-operation-main");
        assert_eq!(
            benchmark.workload_ids().collect::<Vec<_>>(),
            ["checkout-flow"]
        );
        assert!(benchmark.resolve_final_samples(None).is_err());
        assert_eq!(benchmark.resolve_final_samples(Some(20)).unwrap(), 20);
        let resolved: serde_json::Value =
            serde_json::from_str(&benchmark.resolved_json().unwrap()).unwrap();
        assert_eq!(resolved["_source"]["sha256"], benchmark.source_sha256);
    }

    #[test]
    fn built_in_exact_verification_needs_no_process_configuration() {
        let source = BENCHMARK.replace(
            "      command:\n        - node\n        - fixtures/checkout-flow/verify.mjs\n      timeout_seconds: 30\n      identity_files:\n        - fixtures/checkout-flow/verify.mjs",
            "      builtin: exact",
        );
        let benchmark = parse_benchmark(&source).unwrap();
        assert!(matches!(
            benchmark.workload("checkout-flow").unwrap().verifier,
            VerifierInvocation::Exact
        ));
    }

    #[test]
    fn rejects_a_variant_for_another_subject() {
        let benchmark = parse_benchmark(BENCHMARK).unwrap();
        let variant = parse_variant(&VARIANT.replace(
            "subject: browser-operation-adapter",
            "subject: unrelated-parser",
        ))
        .unwrap();
        let error = benchmark.validate_variant(&variant).unwrap_err();
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn rejects_variants_embedded_in_the_benchmark() {
        let changed = BENCHMARK.replace(
            "  subject: browser-operation-adapter",
            "  subject: browser-operation-adapter\n  variants: {}",
        );
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_agent_process_configuration() {
        let changed = BENCHMARK.replace(
            "browser:\n",
            "agent:\n  command: [codex, exec]\n\nbrowser:\n",
        );
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_goal_prompt_configuration() {
        let changed = BENCHMARK.replace(
            "    trace_file:",
            "    goal_file: fixtures/checkout-flow/goal.md\n    trace_file:",
        );
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_unknown_shared_protocol_configuration() {
        let changed = BENCHMARK.replace(
            "  cpu_profile: true",
            "  cpu_profile: true\n  cdp_command: HeapProfiler.enable",
        );
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_host_process_capture_configuration() {
        let changed = BENCHMARK.replace(
            "  cpu_profile: true",
            "  cpu_profile: true\n  process_memory: true",
        );
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_a_duplicate_engine() {
        let changed = BENCHMARK.replace("    - webkit\n", "    - webkit\n    - chromium\n");
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(error.to_string().contains("duplicate engine"));
    }

    #[test]
    fn rejects_a_primary_metric_without_a_threshold() {
        let changed = BENCHMARK.replace("    workload.wall_ms: 5.0\n", "");
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(error.to_string().contains("has no minimum_effect_pct"));
    }

    #[test]
    fn rejects_a_primary_metric_not_produced_by_a_browser_trial() {
        let changed = BENCHMARK
            .replace("    - workload.wall_ms", "    - custom.duration_ms")
            .replace("    workload.wall_ms: 5.0", "    custom.duration_ms: 5.0");
        let error = parse_benchmark(&changed).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("is not produced by a browser trial")
        );
    }

    #[test]
    fn identity_file_content_changes_the_definition_hash() {
        let directory = tempdir().unwrap();
        let implementation = directory.path().join("implementation.js");
        fs::write(&implementation, "export const value = 1;\n").unwrap();
        let first = definition_sha256(
            b"test-definition\0",
            b"descriptor",
            std::iter::once(implementation.as_path()),
        )
        .unwrap();
        fs::write(&implementation, "export const value = 2;\n").unwrap();
        let second = definition_sha256(
            b"test-definition\0",
            b"descriptor",
            std::iter::once(implementation.as_path()),
        )
        .unwrap();
        assert_ne!(first, second);
    }
}
