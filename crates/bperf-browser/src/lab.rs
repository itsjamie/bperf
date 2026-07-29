//! Engine-neutral browser capture evidence and adapter lifecycle.
//!
//! Evidence crosses this boundary only after browser identity and every required
//! artifact pass path, size, and digest validation.

use std::{
    collections::{BTreeMap, HashSet},
    fmt, fs,
    path::Path,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use bperf_runtime::installation::BrowserInstallation;
use serde::{Deserialize, Serialize};

use crate::{
    artifacts::validate_artifacts, chromium::ChromiumAdapter, firefox::FirefoxAdapter,
    webkit::WebKitAdapter,
};

pub const PROTOCOL_VERSION: u32 = 13;
const RUNTIME_ANCHOR_WORKLOAD: &str = "javascript_cpu_v1";
const RUNTIME_ANCHOR_SAMPLES: usize = 31;
const RUNTIME_ANCHOR_MAX_BATCH_SIZE: u32 = 64;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Chromium,
    Firefox,
    Webkit,
}

impl Engine {
    pub const ALL: [Self; 3] = [Self::Chromium, Self::Firefox, Self::Webkit];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::Firefox => "firefox",
            Self::Webkit => "webkit",
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A managed browser-capture session.
///
/// Probes fail when any required capability or artifact is missing; successful
/// evidence always satisfies the complete capture contract.
pub struct BrowserLab {
    chromium: RetainedAdapter<ChromiumAdapter>,
    firefox: RetainedAdapter<FirefoxAdapter>,
    webkit: RetainedAdapter<WebKitAdapter>,
}

impl BrowserLab {
    pub(crate) fn start(installation: BrowserInstallation) -> Result<Self> {
        Ok(Self {
            chromium: RetainedAdapter::new(installation.clone()),
            firefox: RetainedAdapter::new(installation.clone()),
            webkit: RetainedAdapter::new(installation),
        })
    }

    /// Runs one operation with a managed browser session and always attempts to
    /// close every retained engine lane before returning.
    pub fn run<Value>(
        installation: BrowserInstallation,
        operation: impl FnOnce(&mut Self) -> Result<Value>,
    ) -> Result<Value> {
        let mut lab = Self::start(installation)?;
        let result = operation(&mut lab);
        combine_operation_and_shutdown(result, lab.finish())
    }

    /// Proves the complete capture contract for one engine and stores immutable
    /// evidence below `run_root/<engine>`.
    pub fn probe(&mut self, engine: Engine, run_root: &Path) -> Result<CaptureEvidence> {
        let engine_root = run_root.join(engine.as_str());
        fs::create_dir_all(&engine_root)
            .with_context(|| format!("failed to create {}", engine_root.display()))?;
        let engine_root = fs::canonicalize(&engine_root)
            .with_context(|| format!("failed to resolve {}", engine_root.display()))?;

        let evidence = self
            .adapter(engine)
            .probe(&engine_root)?
            .into_evidence(engine);
        validate_evidence(engine, &engine_root, &evidence)?;
        Ok(evidence)
    }

    /// Returns one complete timing, CPU, and heap observation from the same
    /// prepared page and workload execution.
    pub fn measure_trial(&mut self, request: BrowserTrialRequest<'_>) -> Result<TrialEvidence> {
        let BrowserTrialRequest {
            engine,
            artifact_root,
            target_url,
            operations,
            browser,
            batches,
        } = request;
        fs::create_dir_all(artifact_root)
            .with_context(|| format!("failed to create {}", artifact_root.display()))?;
        let artifact_root = fs::canonicalize(artifact_root)
            .with_context(|| format!("failed to resolve {}", artifact_root.display()))?;

        let timed_capture = self.adapter(engine).measure_trial(AdapterTrialRequest {
            artifact_root: &artifact_root,
            target_url,
            operations,
            browser,
            batches,
        })?;
        let evidence = timed_capture.into_evidence();
        validate_trial_evidence(engine, &artifact_root, operations.len(), batches, &evidence)?;
        Ok(evidence)
    }

    pub fn inspect_benchmark(
        &mut self,
        engine: Engine,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection> {
        self.adapter(engine).inspect_benchmark(target_url, case_id)
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        let chromium = self.chromium.finish();
        let firefox = self.firefox.finish();
        let webkit = self.webkit.finish();
        let mut failure: Option<anyhow::Error> = None;
        for (label, result) in [
            ("Rust Chromium adapter", chromium),
            ("Rust Firefox adapter", firefox),
            ("Rust WebKit adapter", webkit),
        ] {
            if let Err(error) = result {
                failure = Some(match failure {
                    None => error,
                    Some(existing) => {
                        existing.context(format!("{label} also failed to close: {error:#}"))
                    }
                });
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn adapter(&mut self, engine: Engine) -> &mut dyn BrowserAdapter {
        match engine {
            Engine::Chromium => &mut self.chromium,
            Engine::Firefox => &mut self.firefox,
            Engine::Webkit => &mut self.webkit,
        }
    }
}

fn combine_operation_and_shutdown<Value>(
    operation: Result<Value>,
    shutdown: Result<()>,
) -> Result<Value> {
    match (operation, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.context("browser adapters failed to close")),
        (Err(error), Err(shutdown)) => Err(error.context(format!(
            "browser adapters also failed to close: {shutdown:#}"
        ))),
    }
}

trait BrowserAdapter {
    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture>;
    fn measure_trial(&mut self, request: AdapterTrialRequest<'_>) -> Result<TimedTrialCapture>;
    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection>;
}

/// Discovers a pinned engine installation and launches browser processes for it.
pub(crate) trait EngineAdapter: Sized {
    type Lane: EngineLane;

    fn discover(installation: &BrowserInstallation) -> Result<Self>;
    fn launch(&self) -> Result<Self::Lane>;
}

/// One retained browser process. Each capture creates and closes isolated page state.
pub(crate) trait EngineLane {
    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture>;
    fn measure_trial(&mut self, request: AdapterTrialRequest<'_>) -> Result<TrialCapture>;
    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection>;
    fn close(&mut self) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
}

struct RetainedAdapter<A: EngineAdapter> {
    installation: BrowserInstallation,
    adapter: Option<A>,
    lane: Option<A::Lane>,
}

impl<A: EngineAdapter> RetainedAdapter<A> {
    fn new(installation: BrowserInstallation) -> Self {
        Self {
            installation,
            adapter: None,
            lane: None,
        }
    }

    fn adapter(&mut self) -> Result<&A> {
        if self.adapter.is_none() {
            self.adapter = Some(A::discover(&self.installation)?);
        }
        Ok(self
            .adapter
            .as_ref()
            .expect("engine adapter was initialized"))
    }

    fn lane(&mut self) -> Result<&mut A::Lane> {
        if self.lane.is_none() {
            let lane = self.adapter()?.launch()?;
            self.lane = Some(lane);
        }
        Ok(self.lane.as_mut().expect("browser lane was initialized"))
    }

    fn with_lane<T>(&mut self, action: impl FnOnce(&mut A::Lane) -> Result<T>) -> Result<T> {
        let result = action(self.lane()?);
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(mut lane) = self.lane.take()
                    && let Err(termination_error) = lane.terminate()
                {
                    return Err(error.context(format!(
                        "browser lane termination also failed: {termination_error:#}"
                    )));
                }
                Err(error)
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(mut lane) = self.lane.take() {
            lane.close()
        } else {
            Ok(())
        }
    }
}

impl<A: EngineAdapter> BrowserAdapter for RetainedAdapter<A> {
    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture> {
        self.with_lane(|lane| lane.probe(artifact_directory))
    }

    fn measure_trial(&mut self, request: AdapterTrialRequest<'_>) -> Result<TimedTrialCapture> {
        self.with_lane(|lane| {
            let started = Instant::now();
            let capture = lane.measure_trial(request)?;
            Ok(TimedTrialCapture {
                elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
                capture,
            })
        })
    }

    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection> {
        self.with_lane(|lane| lane.inspect_benchmark(target_url, case_id))
    }
}

impl<A: EngineAdapter> Drop for RetainedAdapter<A> {
    fn drop(&mut self) {
        if let Some(mut lane) = self.lane.take() {
            let _ = lane.terminate();
        }
    }
}

pub struct BrowserTrialRequest<'a> {
    pub engine: Engine,
    pub artifact_root: &'a Path,
    pub target_url: &'a str,
    pub operations: &'a [serde_json::Value],
    pub browser: &'a BrowserTrialConfig,
    pub batches: TrialBatchConfig,
}

pub(crate) struct AdapterTrialRequest<'a> {
    pub(crate) artifact_root: &'a Path,
    pub(crate) target_url: &'a str,
    pub(crate) operations: &'a [serde_json::Value],
    pub(crate) browser: &'a BrowserTrialConfig,
    pub(crate) batches: TrialBatchConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum AdapterEvidence {
    #[serde(rename = "rust-chromium")]
    Chromium {
        playwright: String,
        chromium_revision: String,
        executable_sha256: String,
        protocol_version: u32,
        browser_workload_version: u32,
    },
    #[serde(rename = "rust-firefox")]
    Firefox {
        playwright: String,
        firefox_revision: String,
        executable_sha256: String,
        protocol_version: u32,
        browser_workload_version: u32,
    },
    #[serde(rename = "rust-webkit")]
    Webkit {
        playwright: String,
        webkit_revision: String,
        executable_sha256: String,
        protocol_version: u32,
        browser_workload_version: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BrowserEvidence {
    pub root_pid: u32,
    pub executable_path: String,
    pub version: String,
    pub launch_args: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureCapabilities {
    pub isolated_launch: bool,
    pub process_root: bool,
    pub cpu_profile: bool,
    pub js_heap: bool,
    pub flamegraph: bool,
}

impl CaptureCapabilities {
    fn required() -> Self {
        Self {
            isolated_launch: true,
            process_root: true,
            cpu_profile: true,
            js_heap: true,
            flamegraph: true,
        }
    }

    fn complete(&self) -> bool {
        self.isolated_launch
            && self.process_root
            && self.cpu_profile
            && self.js_heap
            && self.flamegraph
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    CpuProfile,
    JsHeap,
    Flamegraph,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactEvidence {
    /// Stable trial-local label grouping the native CPU, heap, and flamegraph
    /// files produced by one engine capture scope.
    pub capture_scope: String,
    pub kind: ArtifactKind,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub format: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureEvidence {
    pub engine: Engine,
    pub adapter: AdapterEvidence,
    pub browser: BrowserEvidence,
    pub anchor: RuntimeAnchorEvidence,
    pub capabilities: CaptureCapabilities,
    pub artifacts: Vec<ArtifactEvidence>,
}

pub(crate) struct ProbeCapture {
    pub(crate) adapter: AdapterEvidence,
    pub(crate) browser: BrowserEvidence,
    pub(crate) anchor: RuntimeAnchorEvidence,
    pub(crate) artifacts: Vec<ArtifactEvidence>,
}

impl ProbeCapture {
    fn into_evidence(self, engine: Engine) -> CaptureEvidence {
        CaptureEvidence {
            engine,
            adapter: self.adapter,
            browser: self.browser,
            anchor: self.anchor,
            capabilities: CaptureCapabilities::required(),
            artifacts: self.artifacts,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeAnchorEvidence {
    pub workload: String,
    pub wall_ms: Vec<f64>,
    pub batch_size: u32,
    pub checksum: u32,
}

impl RuntimeAnchorEvidence {
    pub fn validate(&self) -> Result<()> {
        if self.workload != RUNTIME_ANCHOR_WORKLOAD
            || self.wall_ms.len() != RUNTIME_ANCHOR_SAMPLES
            || self
                .wall_ms
                .iter()
                .any(|sample| !sample.is_finite() || *sample <= 0.0)
            || !(1..=RUNTIME_ANCHOR_MAX_BATCH_SIZE).contains(&self.batch_size)
        {
            bail!("invalid runtime anchor evidence");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserTrialConfig {
    pub viewport: Viewport,
    pub locale: String,
    pub timezone_id: String,
    pub color_scheme: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkloadEvidence {
    pub workload_wall_ms: f64,
    pub variant_call_wall_ms: f64,
    pub batch_wall_ms: f64,
    pub batch_size: u32,
    pub operation_count: usize,
    pub result: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TrialEvidence {
    pub capture_elapsed_ms: f64,
    pub workload: WorkloadEvidence,
    pub metrics: BTreeMap<String, f64>,
    pub artifacts: Vec<ArtifactEvidence>,
}

pub(crate) struct TrialCapture {
    pub(crate) workload: WorkloadEvidence,
    pub(crate) cpu_active_ms: f64,
    pub(crate) js_heap_live_bytes: u64,
    pub(crate) artifacts: Vec<ArtifactEvidence>,
}

struct TimedTrialCapture {
    elapsed_ms: f64,
    capture: TrialCapture,
}

impl TimedTrialCapture {
    fn into_evidence(self) -> TrialEvidence {
        let Self {
            elapsed_ms,
            capture:
                TrialCapture {
                    workload,
                    cpu_active_ms,
                    js_heap_live_bytes,
                    artifacts,
                },
        } = self;
        let metrics = BTreeMap::from([
            ("workload.wall_ms".to_owned(), workload.workload_wall_ms),
            (
                "variant.call_wall_ms".to_owned(),
                workload.variant_call_wall_ms,
            ),
            ("browser.cpu_profile.active_ms".to_owned(), cpu_active_ms),
            (
                "browser.js_heap.live_bytes".to_owned(),
                js_heap_live_bytes as f64,
            ),
            ("bperf.capture.elapsed_ms".to_owned(), elapsed_ms),
            (
                "bperf.batch_size".to_owned(),
                f64::from(workload.batch_size),
            ),
        ]);
        TrialEvidence {
            capture_elapsed_ms: elapsed_ms,
            workload,
            metrics,
            artifacts,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TrialBatchConfig {
    size: u32,
    target_ms: Option<f64>,
    max_size: u32,
}

impl TrialBatchConfig {
    pub const SINGLE: Self = Self::fixed(1);

    pub const fn fixed(size: u32) -> Self {
        Self {
            size,
            target_ms: None,
            max_size: size,
        }
    }

    pub const fn calibrating(target_ms: f64, max_size: u32) -> Self {
        Self {
            size: 1,
            target_ms: Some(target_ms),
            max_size,
        }
    }

    fn accepts(self, workload: &WorkloadEvidence) -> bool {
        self.target_ms
            .map_or(workload.batch_size == self.size, |_| {
                workload.batch_size >= self.size && workload.batch_size <= self.max_size
            })
    }

    pub(crate) const fn initial_size(self) -> u32 {
        self.size
    }

    pub(crate) const fn target_ms(self) -> Option<f64> {
        self.target_ms
    }

    pub(crate) const fn max_size(self) -> u32 {
        self.max_size
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchmarkInspection {
    pub description: serde_json::Value,
    pub result: Option<serde_json::Value>,
}

fn validate_evidence(
    expected_engine: Engine,
    root: &Path,
    evidence: &CaptureEvidence,
) -> Result<()> {
    if evidence.engine != expected_engine {
        bail!(
            "browser adapter returned engine {} while {} was requested",
            evidence.engine,
            expected_engine
        );
    }
    validate_adapter(expected_engine, &evidence.adapter)?;
    validate_browser(expected_engine, &evidence.browser)?;
    validate_anchor(expected_engine, &evidence.anchor)?;
    if !evidence.capabilities.complete() {
        bail!("{expected_engine} did not satisfy every required capability");
    }
    validate_artifacts(expected_engine, root, &evidence.artifacts)
}

fn validate_anchor(engine: Engine, anchor: &RuntimeAnchorEvidence) -> Result<()> {
    anchor
        .validate()
        .with_context(|| format!("{engine} returned invalid runtime anchor evidence"))
}

fn validate_browser(engine: Engine, browser: &BrowserEvidence) -> Result<()> {
    if browser.root_pid == 0 {
        bail!("{engine} did not expose a browser root PID");
    }
    if browser.executable_path.is_empty() || browser.version.is_empty() {
        bail!("{engine} browser identity is incomplete");
    }
    Ok(())
}

fn validate_adapter(engine: Engine, adapter: &AdapterEvidence) -> Result<()> {
    match (engine, adapter) {
        (
            Engine::Firefox,
            AdapterEvidence::Firefox {
                playwright,
                firefox_revision,
                executable_sha256,
                protocol_version,
                browser_workload_version,
            },
        ) if !playwright.is_empty()
            && !firefox_revision.is_empty()
            && is_sha256(executable_sha256)
            && *protocol_version > 0
            && *browser_workload_version > 0 =>
        {
            Ok(())
        }
        (
            Engine::Chromium,
            AdapterEvidence::Chromium {
                playwright,
                chromium_revision,
                executable_sha256,
                protocol_version,
                browser_workload_version,
            },
        ) if !playwright.is_empty()
            && !chromium_revision.is_empty()
            && is_sha256(executable_sha256)
            && *protocol_version > 0
            && *browser_workload_version > 0 =>
        {
            Ok(())
        }
        (
            Engine::Webkit,
            AdapterEvidence::Webkit {
                playwright,
                webkit_revision,
                executable_sha256,
                protocol_version,
                browser_workload_version,
            },
        ) if !playwright.is_empty()
            && !webkit_revision.is_empty()
            && is_sha256(executable_sha256)
            && *protocol_version > 0
            && *browser_workload_version > 0 =>
        {
            Ok(())
        }
        _ => bail!("{engine} adapter identity is incomplete or has the wrong adapter kind"),
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_trial_evidence(
    engine: Engine,
    root: &Path,
    operation_count: usize,
    batches: TrialBatchConfig,
    evidence: &TrialEvidence,
) -> Result<()> {
    let workload = &evidence.workload;
    if workload.operation_count != operation_count || workload.result.len() != operation_count {
        bail!("{engine} trial returned inconsistent workload evidence");
    }
    if !evidence.capture_elapsed_ms.is_finite()
        || evidence.capture_elapsed_ms <= 0.0
        || !workload.workload_wall_ms.is_finite()
        || workload.workload_wall_ms <= 0.0
        || !workload.variant_call_wall_ms.is_finite()
        || workload.variant_call_wall_ms <= 0.0
        || workload.variant_call_wall_ms > workload.workload_wall_ms
        || !workload.batch_wall_ms.is_finite()
        || workload.batch_wall_ms < workload.workload_wall_ms
    {
        bail!(
            "{engine} trial returned invalid timing \
             (workload_wall_ms={}, variant_call_wall_ms={}, batch_wall_ms={})",
            workload.workload_wall_ms,
            workload.variant_call_wall_ms,
            workload.batch_wall_ms,
        );
    }
    if !batches.accepts(workload) {
        bail!("{engine} trial did not honor its batch plan");
    }

    let required_metrics = HashSet::from([
        "workload.wall_ms",
        "variant.call_wall_ms",
        "browser.cpu_profile.active_ms",
        "browser.js_heap.live_bytes",
        "bperf.capture.elapsed_ms",
        "bperf.batch_size",
    ]);
    let actual_metrics: HashSet<_> = evidence.metrics.keys().map(String::as_str).collect();
    if actual_metrics != required_metrics {
        bail!("{engine} trial returned invalid metrics: {actual_metrics:?}");
    }
    for metric in &required_metrics {
        let value = evidence
            .metrics
            .get(*metric)
            .with_context(|| format!("{engine} trial returned no metric {metric:?}"))?;
        if !value.is_finite() || *value <= 0.0 {
            bail!("{engine} trial metric {metric:?} must be finite and positive");
        }
    }

    if evidence.metrics["workload.wall_ms"] != workload.workload_wall_ms
        || evidence.metrics["variant.call_wall_ms"] != workload.variant_call_wall_ms
        || evidence.metrics["bperf.capture.elapsed_ms"] != evidence.capture_elapsed_ms
        || evidence.metrics["bperf.batch_size"] != f64::from(workload.batch_size)
    {
        bail!("{engine} trial metrics do not match their capture evidence");
    }
    validate_artifacts(engine, root, &evidence.artifacts)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::*;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GoldenCapture {
        schema_version: u32,
        engine: Engine,
        artifacts: Vec<ArtifactEvidence>,
    }

    static DISCOVERIES: AtomicUsize = AtomicUsize::new(0);
    static LAUNCHES: AtomicUsize = AtomicUsize::new(0);
    static CLOSES: AtomicUsize = AtomicUsize::new(0);
    static TERMINATIONS: AtomicUsize = AtomicUsize::new(0);

    struct FakeAdapter;

    impl EngineAdapter for FakeAdapter {
        type Lane = FakeLane;

        fn discover(_installation: &BrowserInstallation) -> Result<Self> {
            DISCOVERIES.fetch_add(1, Ordering::SeqCst);
            Ok(Self)
        }

        fn launch(&self) -> Result<Self::Lane> {
            Ok(FakeLane {
                id: LAUNCHES.fetch_add(1, Ordering::SeqCst) + 1,
            })
        }
    }

    struct FakeLane {
        id: usize,
    }

    impl EngineLane for FakeLane {
        fn probe(&mut self, _artifact_directory: &Path) -> Result<ProbeCapture> {
            unreachable!("probe is outside this lifecycle test")
        }

        fn measure_trial(&mut self, _request: AdapterTrialRequest<'_>) -> Result<TrialCapture> {
            unreachable!("trial capture is outside this lifecycle test")
        }

        fn inspect_benchmark(
            &mut self,
            target_url: &str,
            _case_id: Option<&str>,
        ) -> Result<BenchmarkInspection> {
            if target_url == "fail" {
                bail!("scripted lane failure");
            }
            Ok(BenchmarkInspection {
                description: json!({"lane": self.id}),
                result: None,
            })
        }

        fn close(&mut self) -> Result<()> {
            CLOSES.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            TERMINATIONS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn golden_capture_fixtures_satisfy_every_engine_contract() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("captures");
        for engine in Engine::ALL {
            let fixture_root = root.join(engine.as_str());
            let fixture: GoldenCapture =
                serde_json::from_slice(&fs::read(fixture_root.join("capture.json")).unwrap())
                    .unwrap();
            assert_eq!(fixture.schema_version, 2);
            assert_eq!(fixture.engine, engine);
            validate_artifacts(engine, &fixture_root, &fixture.artifacts).unwrap();
        }
    }

    fn valid_evidence(root: &Path, engine: Engine) -> CaptureEvidence {
        let mut artifacts = Vec::new();
        for (kind, name) in [
            (ArtifactKind::CpuProfile, "cpu.json"),
            (ArtifactKind::JsHeap, "heap.json"),
            (ArtifactKind::Flamegraph, "flamegraph.json"),
        ] {
            let bytes = format!("artifact-{name}").into_bytes();
            fs::write(root.join(name), &bytes).unwrap();
            artifacts.push(ArtifactEvidence {
                capture_scope: crate::artifacts::default_capture_scope(engine).to_owned(),
                kind,
                path: name.to_owned(),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                format: "test".to_owned(),
            });
        }
        CaptureEvidence {
            engine,
            adapter: match engine {
                Engine::Chromium => AdapterEvidence::Chromium {
                    playwright: "1.61.1".to_owned(),
                    chromium_revision: "1228".to_owned(),
                    executable_sha256: "a".repeat(64),
                    protocol_version: 1,
                    browser_workload_version: 1,
                },
                Engine::Firefox => AdapterEvidence::Firefox {
                    playwright: "1.61.1".to_owned(),
                    firefox_revision: "1532".to_owned(),
                    executable_sha256: "a".repeat(64),
                    protocol_version: 1,
                    browser_workload_version: 1,
                },
                Engine::Webkit => AdapterEvidence::Webkit {
                    playwright: "1.61.1".to_owned(),
                    webkit_revision: "2311".to_owned(),
                    executable_sha256: "b".repeat(64),
                    protocol_version: 1,
                    browser_workload_version: 1,
                },
            },
            browser: BrowserEvidence {
                root_pid: 42,
                executable_path: "browser.exe".to_owned(),
                version: "1".to_owned(),
                launch_args: Vec::new(),
            },
            anchor: RuntimeAnchorEvidence {
                workload: RUNTIME_ANCHOR_WORKLOAD.to_owned(),
                wall_ms: vec![10.0; RUNTIME_ANCHOR_SAMPLES],
                batch_size: 1,
                checksum: 42,
            },
            capabilities: CaptureCapabilities {
                isolated_launch: true,
                process_root: true,
                cpu_profile: true,
                js_heap: true,
                flamegraph: true,
            },
            artifacts,
        }
    }

    fn valid_trial_evidence(root: &Path, engine: Engine, batch_size: u32) -> TrialEvidence {
        let capture = valid_evidence(root, engine);
        let workload = WorkloadEvidence {
            workload_wall_ms: 1.0,
            variant_call_wall_ms: 0.5,
            batch_wall_ms: f64::from(batch_size),
            batch_size,
            operation_count: 1,
            result: vec![json!({"value": 42})],
        };
        TimedTrialCapture {
            elapsed_ms: 5.0,
            capture: TrialCapture {
                workload,
                cpu_active_ms: 2.0,
                js_heap_live_bytes: 4_096,
                artifacts: capture.artifacts,
            },
        }
        .into_evidence()
    }

    #[test]
    fn engine_names_are_stable() {
        assert_eq!(
            Engine::ALL.map(Engine::as_str),
            ["chromium", "firefox", "webkit"]
        );
    }

    #[test]
    fn operation_and_shutdown_failures_are_both_preserved() {
        let error = combine_operation_and_shutdown::<()>(
            Err(anyhow::anyhow!("measurement failed")),
            Err(anyhow::anyhow!("cleanup failed")),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("measurement failed"));
        assert!(message.contains("cleanup failed"));

        let shutdown_only =
            combine_operation_and_shutdown(Ok("complete"), Err(anyhow::anyhow!("cleanup failed")))
                .unwrap_err();
        assert!(format!("{shutdown_only:#}").contains("browser adapters failed to close"));
    }

    #[test]
    fn retained_adapter_reuses_healthy_lanes_and_reopens_poisoned_lanes() {
        for counter in [&DISCOVERIES, &LAUNCHES, &CLOSES, &TERMINATIONS] {
            counter.store(0, Ordering::SeqCst);
        }
        let mut adapter =
            RetainedAdapter::<FakeAdapter>::new(BrowserInstallation::discover().unwrap());
        assert_eq!(DISCOVERIES.load(Ordering::SeqCst), 0);

        let first = adapter.inspect_benchmark("ok", None).unwrap();
        let second = adapter.inspect_benchmark("ok", None).unwrap();
        assert_eq!(first.description["lane"], 1);
        assert_eq!(second.description["lane"], 1);
        assert_eq!(DISCOVERIES.load(Ordering::SeqCst), 1);
        assert_eq!(LAUNCHES.load(Ordering::SeqCst), 1);

        adapter.inspect_benchmark("fail", None).unwrap_err();
        assert!(adapter.lane.is_none());
        assert_eq!(TERMINATIONS.load(Ordering::SeqCst), 1);

        let reopened = adapter.inspect_benchmark("ok", None).unwrap();
        assert_eq!(reopened.description["lane"], 2);
        assert_eq!(DISCOVERIES.load(Ordering::SeqCst), 1);
        assert_eq!(LAUNCHES.load(Ordering::SeqCst), 2);

        adapter.finish().unwrap();
        assert_eq!(CLOSES.load(Ordering::SeqCst), 1);
        assert_eq!(TERMINATIONS.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[ignore = "launches all three pinned Playwright browsers"]
    fn retained_lanes_keep_one_root_pid_and_shutdown_all_contained_processes() {
        let artifacts = tempdir().unwrap();
        let mut browser_lab = BrowserLab::start(BrowserInstallation::discover().unwrap()).unwrap();

        for engine in Engine::ALL {
            let first = browser_lab
                .probe(engine, &artifacts.path().join("first"))
                .unwrap();
            let second = browser_lab
                .probe(engine, &artifacts.path().join("second"))
                .unwrap();
            assert_eq!(
                first.browser.root_pid, second.browser.root_pid,
                "{engine} replaced its healthy retained browser process"
            );
        }

        // BrowserProcess rejects shutdown until the process group or Job Object
        // reports that no browser root or descendant remains.
        browser_lab.finish().unwrap();
    }

    #[test]
    fn calibration_accepts_a_batch_selected_within_its_bounds() {
        let batches = TrialBatchConfig::calibrating(100.0, 10_000);
        assert!(batches.accepts(&WorkloadEvidence {
            workload_wall_ms: 1.0,
            variant_call_wall_ms: 0.5,
            batch_wall_ms: 5_000.0,
            batch_size: 5_000,
            operation_count: 1,
            result: vec![json!({"value": 42})],
        }));
    }

    #[test]
    fn accepts_complete_evidence_for_every_engine() {
        for engine in Engine::ALL {
            let directory = tempdir().unwrap();
            let evidence = valid_evidence(directory.path(), engine);
            validate_evidence(engine, directory.path(), &evidence).unwrap();
        }
    }

    #[test]
    fn accepts_complete_trial_evidence_for_every_engine() {
        for engine in Engine::ALL {
            let directory = tempdir().unwrap();
            let evidence = valid_trial_evidence(directory.path(), engine, 3);
            validate_trial_evidence(
                engine,
                directory.path(),
                1,
                TrialBatchConfig::fixed(3),
                &evidence,
            )
            .unwrap();
        }
    }

    #[test]
    fn rejects_an_incomplete_trial_artifact_set() {
        let directory = tempdir().unwrap();
        let mut evidence = valid_trial_evidence(directory.path(), Engine::Chromium, 3);
        evidence.artifacts.pop();

        let error = validate_trial_evidence(
            Engine::Chromium,
            directory.path(),
            1,
            TrialBatchConfig::fixed(3),
            &evidence,
        )
        .unwrap_err();
        assert!(error.to_string().contains("artifact scope"));
    }

    #[test]
    fn rejects_missing_artifact_kind() {
        let directory = tempdir().unwrap();
        let mut evidence = valid_evidence(directory.path(), Engine::Chromium);
        evidence.artifacts.pop();
        let error = validate_evidence(Engine::Chromium, directory.path(), &evidence).unwrap_err();
        assert!(error.to_string().contains("artifact scope"));
    }

    #[test]
    fn rejects_tampered_artifact() {
        let directory = tempdir().unwrap();
        let evidence = valid_evidence(directory.path(), Engine::Chromium);
        fs::write(directory.path().join("cpu.json"), b"tampered").unwrap();
        let error = validate_evidence(Engine::Chromium, directory.path(), &evidence).unwrap_err();
        assert!(
            error.to_string().contains("size mismatch")
                || error.to_string().contains("hash mismatch")
        );
    }

    #[test]
    fn rejects_an_engine_substitution() {
        let directory = tempdir().unwrap();
        let evidence = valid_evidence(directory.path(), Engine::Chromium);
        let error = validate_evidence(Engine::Firefox, directory.path(), &evidence).unwrap_err();
        assert!(error.to_string().contains("while firefox was requested"));
    }
}
