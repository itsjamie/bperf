//! Browser capture evidence and sidecar lifecycle.
//!
//! Evidence crosses this boundary only after browser identity and every required
//! artifact pass path, size, and digest validation.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::sidecar_runtime::{SidecarInstallation, node_path};

pub const PROTOCOL_VERSION: u32 = 9;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
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

/// Selects the Node executable and sidecar entrypoint used by [`BrowserLab`].
pub struct BrowserLabConfig {
    node: PathBuf,
    sidecar: PathBuf,
}

impl BrowserLabConfig {
    pub fn discover() -> Result<Self> {
        let node = std::env::var_os("BPERF_NODE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("node"));
        let sidecar = SidecarInstallation::discover()?.capture_entrypoint();
        Ok(Self { node, sidecar })
    }

    pub fn with_node(mut self, node: impl Into<PathBuf>) -> Self {
        self.node = node.into();
        self
    }

    pub fn with_sidecar(mut self, sidecar: impl Into<PathBuf>) -> Self {
        self.sidecar = sidecar.into();
        self
    }
}

/// A managed browser-capture session.
///
/// Probes fail when any required capability or artifact is missing; successful
/// evidence always satisfies the complete capture contract.
pub struct BrowserLab {
    transport: ProcessTransport,
}

impl BrowserLab {
    pub fn start(config: BrowserLabConfig) -> Result<Self> {
        if !config.sidecar.is_file() {
            bail!("Node sidecar does not exist: {}", config.sidecar.display());
        }
        Ok(Self {
            transport: ProcessTransport::spawn(&config.node, &config.sidecar)?,
        })
    }

    /// Proves the complete capture contract for one engine and stores immutable
    /// evidence below `run_root/<engine>`.
    pub fn probe(&mut self, engine: Engine, run_root: &Path) -> Result<CaptureEvidence> {
        let engine_root = run_root.join(engine.as_str());
        fs::create_dir_all(&engine_root)
            .with_context(|| format!("failed to create {}", engine_root.display()))?;
        let engine_root = fs::canonicalize(&engine_root)
            .with_context(|| format!("failed to resolve {}", engine_root.display()))?;

        let evidence: CaptureEvidence = self.transport.request(
            "doctor",
            WireDoctorParams {
                engine,
                artifact_dir: engine_root.to_string_lossy().into_owned(),
            },
        )?;
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

        let evidence: TrialEvidence = self.transport.request(
            "measure_trial",
            WireTrialParams {
                engine,
                artifact_dir: artifact_root.to_string_lossy().into_owned(),
                target_url,
                operations,
                browser,
                batch_size: batches.size,
                batch_target_ms: batches.target_ms,
                batch_max_size: batches.max_size,
            },
        )?;
        validate_trial_evidence(engine, &artifact_root, operations.len(), batches, &evidence)?;
        Ok(evidence)
    }

    pub fn finish(mut self) -> Result<()> {
        self.transport.shutdown()
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeEvidence {
    pub node: String,
    pub playwright: String,
    pub platform: String,
    pub arch: String,
    pub os_release: String,
    pub cpu_model: String,
    pub logical_cpus: u32,
    pub total_memory_bytes: u64,
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
    pub kind: ArtifactKind,
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub format: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureEvidence {
    pub engine: Engine,
    pub runtime: RuntimeEvidence,
    pub browser: BrowserEvidence,
    pub anchor: RuntimeAnchorEvidence,
    pub capabilities: CaptureCapabilities,
    pub artifacts: Vec<ArtifactEvidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuntimeAnchorEvidence {
    pub workload: String,
    pub wall_ms: Vec<f64>,
    pub batch_size: u32,
    pub checksum: u32,
}

impl RuntimeAnchorEvidence {
    pub(crate) fn validate(&self) -> Result<()> {
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
    pub engine: Engine,
    pub runtime: RuntimeEvidence,
    pub browser: BrowserEvidence,
    pub capture_elapsed_ms: f64,
    pub workload: WorkloadEvidence,
    pub metrics: BTreeMap<String, f64>,
    pub artifacts: Vec<ArtifactEvidence>,
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
}

fn validate_evidence(
    expected_engine: Engine,
    root: &Path,
    evidence: &CaptureEvidence,
) -> Result<()> {
    if evidence.engine != expected_engine {
        bail!(
            "sidecar returned engine {} while {} was requested",
            evidence.engine,
            expected_engine
        );
    }
    validate_runtime(expected_engine, &evidence.runtime)?;
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

fn validate_runtime(engine: Engine, runtime: &RuntimeEvidence) -> Result<()> {
    if runtime.node.is_empty()
        || runtime.playwright.is_empty()
        || runtime.platform.is_empty()
        || runtime.arch.is_empty()
        || runtime.os_release.is_empty()
        || runtime.cpu_model.is_empty()
        || runtime.logical_cpus == 0
        || runtime.total_memory_bytes == 0
    {
        bail!("{engine} runtime identity is incomplete");
    }
    Ok(())
}

pub(crate) fn validate_artifacts(
    engine: Engine,
    root: &Path,
    artifacts: &[ArtifactEvidence],
) -> Result<()> {
    validate_artifact_set(engine, artifacts)?;
    validate_artifact_files(engine, root, artifacts)
}

pub(crate) fn validate_artifact_set(engine: Engine, artifacts: &[ArtifactEvidence]) -> Result<()> {
    let expected_kinds = HashSet::from([
        ArtifactKind::CpuProfile,
        ArtifactKind::JsHeap,
        ArtifactKind::Flamegraph,
    ]);
    validate_artifact_set_for(engine, artifacts, &expected_kinds)
}

pub(crate) fn validate_trial_artifact_set(
    engine: Engine,
    artifacts: &[ArtifactEvidence],
) -> Result<()> {
    validate_artifact_set(engine, artifacts)
}

fn validate_artifact_set_for(
    engine: Engine,
    artifacts: &[ArtifactEvidence],
    expected_kinds: &HashSet<ArtifactKind>,
) -> Result<()> {
    let actual_kinds: HashSet<_> = artifacts.iter().map(|artifact| artifact.kind).collect();
    if actual_kinds != *expected_kinds || artifacts.len() != expected_kinds.len() {
        bail!("{engine} returned an invalid artifact set: {actual_kinds:?}");
    }

    for artifact in artifacts {
        let relative = Path::new(&artifact.path);
        if artifact.path.trim().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!(
                "{engine} artifact path must be contained: {}",
                artifact.path
            );
        }
        if artifact.size_bytes == 0
            || artifact.format.trim().is_empty()
            || artifact.sha256.len() != 64
            || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("{engine} artifact descriptor is invalid: {}", artifact.path);
        }
    }
    Ok(())
}

pub(crate) fn validate_artifact_files(
    engine: Engine,
    root: &Path,
    artifacts: &[ArtifactEvidence],
) -> Result<()> {
    let canonical_root = fs::canonicalize(root)?;
    for artifact in artifacts {
        let relative = Path::new(&artifact.path);
        let canonical_path = fs::canonicalize(canonical_root.join(relative))
            .with_context(|| format!("artifact does not exist: {}", artifact.path))?;
        if !canonical_path.starts_with(&canonical_root) {
            bail!(
                "{engine} artifact escaped its artifact directory: {}",
                artifact.path
            );
        }
        let actual_size = fs::metadata(&canonical_path)?.len();
        if actual_size == 0 || actual_size != artifact.size_bytes {
            bail!(
                "{engine} artifact size mismatch for {}: reported {}, actual {}",
                artifact.path,
                artifact.size_bytes,
                actual_size
            );
        }
        if sha256(&canonical_path)? != artifact.sha256 {
            bail!("{engine} artifact hash mismatch for {}", artifact.path);
        }
    }
    Ok(())
}

fn validate_trial_evidence(
    expected_engine: Engine,
    root: &Path,
    operation_count: usize,
    batches: TrialBatchConfig,
    evidence: &TrialEvidence,
) -> Result<()> {
    if evidence.engine != expected_engine {
        bail!(
            "sidecar returned engine {} while {} was requested",
            evidence.engine,
            expected_engine
        );
    }
    validate_runtime(expected_engine, &evidence.runtime)?;
    validate_browser(expected_engine, &evidence.browser)?;
    let workload = &evidence.workload;
    if workload.operation_count != operation_count || workload.result.len() != operation_count {
        bail!("{expected_engine} trial returned inconsistent workload evidence");
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
            "{expected_engine} trial returned invalid timing \
             (workload_wall_ms={}, variant_call_wall_ms={}, batch_wall_ms={})",
            workload.workload_wall_ms,
            workload.variant_call_wall_ms,
            workload.batch_wall_ms,
        );
    }
    if !batches.accepts(workload) {
        bail!("{expected_engine} trial did not honor its batch plan");
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
        bail!("{expected_engine} trial returned invalid metrics: {actual_metrics:?}");
    }
    for metric in &required_metrics {
        let value = evidence
            .metrics
            .get(*metric)
            .with_context(|| format!("{expected_engine} trial returned no metric {metric:?}"))?;
        if !value.is_finite() || *value <= 0.0 {
            bail!("{expected_engine} trial metric {metric:?} must be finite and positive");
        }
    }

    if evidence.metrics["workload.wall_ms"] != workload.workload_wall_ms
        || evidence.metrics["variant.call_wall_ms"] != workload.variant_call_wall_ms
        || evidence.metrics["bperf.capture.elapsed_ms"] != evidence.capture_elapsed_ms
        || evidence.metrics["bperf.batch_size"] != f64::from(workload.batch_size)
    {
        bail!("{expected_engine} trial metrics do not match their capture evidence");
    }
    validate_trial_artifact_set(expected_engine, &evidence.artifacts)?;
    validate_artifact_files(expected_engine, root, &evidence.artifacts)
}

fn sha256(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Debug, Serialize)]
struct WireRequest<P> {
    protocol_version: u32,
    id: u64,
    method: &'static str,
    params: P,
}

#[derive(Debug, Deserialize)]
struct WireResponse<T> {
    protocol_version: u32,
    id: u64,
    ok: bool,
    result: Option<T>,
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    code: String,
    message: String,
    #[serde(default)]
    stack: Option<String>,
}

#[derive(Debug, Serialize)]
struct WireDoctorParams {
    engine: Engine,
    artifact_dir: String,
}

#[derive(Debug, Serialize)]
struct WireTrialParams<'a> {
    engine: Engine,
    artifact_dir: String,
    target_url: &'a str,
    operations: &'a [serde_json::Value],
    browser: &'a BrowserTrialConfig,
    batch_size: u32,
    batch_target_ms: Option<f64>,
    batch_max_size: u32,
}

struct ProcessTransport {
    child: Child,
    stdin: ChildStdin,
    stdout_lines: mpsc::Receiver<std::io::Result<String>>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    next_id: u64,
}

impl ProcessTransport {
    fn spawn(node: &Path, script: &Path) -> Result<Self> {
        let mut child = Command::new(node)
            .arg(node_path(script))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start browser laboratory with Node executable {}",
                    node.display()
                )
            })?;

        let stdin = child
            .stdin
            .take()
            .context("browser laboratory stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("browser laboratory stdout was unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("browser laboratory stderr was unavailable")?;
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));
        let captured_lines = Arc::clone(&stderr_lines);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(|line| line.ok()) {
                eprintln!("[browser-lab] {line}");
                if let Ok(mut lines) = captured_lines.lock() {
                    lines.push(line);
                }
            }
        });
        let (stdout_sender, stdout_lines) = mpsc::sync_channel(16);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if stdout_sender.send(line).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            stdout_lines,
            stderr_lines,
            next_id: 1,
        })
    }

    fn request<P, T>(&mut self, method: &'static str, params: P) -> Result<T>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT)
    }

    fn request_with_timeout<P, T>(
        &mut self,
        method: &'static str,
        params: P,
        timeout: Duration,
    ) -> Result<T>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        let id = self.next_id;
        self.next_id += 1;
        let request = WireRequest {
            protocol_version: PROTOCOL_VERSION,
            id,
            method,
            params,
        };
        serde_json::to_writer(&mut self.stdin, &request)
            .context("failed to encode browser laboratory request")?;
        self.stdin
            .write_all(b"\n")
            .context("failed to terminate browser laboratory request")?;
        self.stdin
            .flush()
            .context("failed to flush browser laboratory request")?;

        let line = match self.stdout_lines.recv_timeout(timeout) {
            Ok(line) => line.context("failed to read browser laboratory response")?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                bail!(
                    "browser laboratory did not respond within {timeout:?}{}",
                    format_stderr(&self.stderr_snapshot())
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = self.child.try_wait().ok().flatten();
                bail!(
                    "browser laboratory closed before responding (status: {status:?}){}",
                    format_stderr(&self.stderr_snapshot())
                )
            }
        };

        let response: WireResponse<T> =
            serde_json::from_str(&line).context("browser laboratory emitted invalid JSON")?;
        if response.protocol_version != PROTOCOL_VERSION {
            bail!(
                "browser laboratory protocol mismatch: core={}, adapter={}",
                PROTOCOL_VERSION,
                response.protocol_version
            );
        }
        if response.id != id {
            bail!(
                "browser laboratory response id mismatch: expected {id}, received {}",
                response.id
            );
        }
        if !response.ok {
            let error = response.error.unwrap_or(WireError {
                code: "unknown".to_owned(),
                message: "request failed without an error body".to_owned(),
                stack: None,
            });
            bail!(
                "browser laboratory {}: {}{}",
                error.code,
                error.message,
                error
                    .stack
                    .map(|stack| format!("\n{stack}"))
                    .unwrap_or_default()
            );
        }
        response
            .result
            .context("successful browser laboratory response had no evidence")
    }

    fn shutdown(&mut self) -> Result<()> {
        let _: serde_json::Value =
            self.request_with_timeout("shutdown", serde_json::json!({}), SHUTDOWN_TIMEOUT)?;
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let status = loop {
            if let Some(status) = self
                .child
                .try_wait()
                .context("failed waiting for browser laboratory")?
            {
                break status;
            }
            if Instant::now() >= deadline {
                self.child
                    .kill()
                    .context("browser laboratory ignored shutdown and could not be stopped")?;
                let _ = self.child.wait();
                bail!("browser laboratory did not exit after its shutdown response");
            }
            thread::sleep(Duration::from_millis(10));
        };
        if !status.success() {
            bail!(
                "browser laboratory exited with {status}{}",
                format_stderr(&self.stderr_snapshot())
            );
        }
        Ok(())
    }

    fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr_lines
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default()
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn format_stderr(lines: &[String]) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        format!("\nbrowser laboratory stderr:\n{}", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GoldenCapture {
        schema_version: u32,
        engine: Engine,
        artifacts: Vec<ArtifactEvidence>,
    }

    #[test]
    fn golden_capture_fixtures_satisfy_every_engine_contract() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("sidecar")
            .join("test")
            .join("fixtures")
            .join("captures");
        for engine in Engine::ALL {
            let fixture_root = root.join(engine.as_str());
            let fixture: GoldenCapture =
                serde_json::from_slice(&fs::read(fixture_root.join("capture.json")).unwrap())
                    .unwrap();
            assert_eq!(fixture.schema_version, 1);
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
                kind,
                path: name.to_owned(),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                format: "test".to_owned(),
            });
        }
        CaptureEvidence {
            engine,
            runtime: RuntimeEvidence {
                node: "v24".to_owned(),
                playwright: "1.61.1".to_owned(),
                platform: "win32".to_owned(),
                arch: "x64".to_owned(),
                os_release: "10.0.0".to_owned(),
                cpu_model: "test cpu".to_owned(),
                logical_cpus: 8,
                total_memory_bytes: 16 * 1024 * 1024 * 1024,
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
        TrialEvidence {
            engine,
            runtime: capture.runtime,
            browser: capture.browser,
            capture_elapsed_ms: 5.0,
            workload,
            metrics: BTreeMap::from([
                ("workload.wall_ms".to_owned(), 1.0),
                ("variant.call_wall_ms".to_owned(), 0.5),
                ("browser.cpu_profile.active_ms".to_owned(), 2.0),
                ("browser.js_heap.live_bytes".to_owned(), 4_096.0),
                ("bperf.capture.elapsed_ms".to_owned(), 5.0),
                ("bperf.batch_size".to_owned(), f64::from(batch_size)),
            ]),
            artifacts: capture.artifacts,
        }
    }

    #[test]
    fn engine_names_are_stable() {
        assert_eq!(
            Engine::ALL.map(Engine::as_str),
            ["chromium", "firefox", "webkit"]
        );
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
        assert!(error.to_string().contains("invalid artifact set"));
    }

    #[test]
    fn rejects_missing_artifact_kind() {
        let directory = tempdir().unwrap();
        let mut evidence = valid_evidence(directory.path(), Engine::Chromium);
        evidence.artifacts.pop();
        let error = validate_evidence(Engine::Chromium, directory.path(), &evidence).unwrap_err();
        assert!(error.to_string().contains("invalid artifact set"));
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
