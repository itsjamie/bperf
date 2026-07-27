//! Direct adapter for Playwright's patched WebKit build.
//!
//! Browser discovery, process ownership, private inspector routing, workload
//! execution, native capture parsing, and artifact normalization stay behind
//! the engine-neutral browser laboratory interface.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bperf_runtime::installation::{BrowserName, RuntimeInstallation};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    artifacts::{
        CaptureArtifacts, SpeedscopeBuilder, SpeedscopeDocument, SpeedscopeFrame, positive_weights,
        sha256_file,
    },
    browser_process::BrowserProcess,
    browser_workload::{
        BENCHMARK_DESCRIPTION_EXPRESSION, BENCHMARK_READY_EXPRESSION, DOCTOR_PROBE_EXPRESSION,
        RUNTIME_ANCHOR_EXPRESSION, SETTLE_EXPRESSION, VERSION as BROWSER_WORKLOAD_VERSION,
        WORKLOAD_READY_EXPRESSION, WorkloadScript, bootstrap_source, decode_batch_size,
        decode_runtime_anchor, decode_workload, default_browser_config, installed_expression,
        is_allowed_adapter_url, is_allowed_trial_url, is_benchmark_code_url,
    },
    lab::{
        AdapterEvidence, AdapterTrialRequest, ArtifactEvidence, BenchmarkInspection,
        BrowserEvidence, BrowserTrialConfig, Engine, EngineAdapter, EngineLane, ProbeCapture,
        TrialCapture,
    },
};

pub(crate) const ADAPTER_PROTOCOL_VERSION: u32 = 2;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(300);
const PAGE_READY_TIMEOUT: Duration = Duration::from_secs(10);
const REQUIRED_PROTOCOL_COMMANDS: &[&str] = &[
    "Dialog.enable",
    "Emulation.setActiveAndFocused",
    "Emulation.setDeviceMetricsOverride",
    "Heap.enable",
    "Heap.snapshot",
    "Network.addInterception",
    "Network.enable",
    "Network.interceptRequestWithError",
    "Network.interceptWithRequest",
    "Network.setExtraHTTPHeaders",
    "Network.setInterceptionEnabled",
    "Network.setResourceCachingDisabled",
    "Page.enable",
    "Page.getResourceTree",
    "Page.overrideSetting",
    "Page.overrideUserAgent",
    "Page.overrideUserPreference",
    "Page.setBootstrapScript",
    "Page.setEmulatedMedia",
    "Page.setForcedColors",
    "Page.setScreenSizeOverride",
    "Page.setTimeZone",
    "Page.setTouchEmulationEnabled",
    "Playwright.close",
    "Playwright.createContext",
    "Playwright.createPage",
    "Playwright.deleteContext",
    "Playwright.enable",
    "Playwright.navigate",
    "Playwright.setDownloadBehavior",
    "Playwright.setLanguages",
    "Runtime.awaitPromise",
    "Runtime.enable",
    "Runtime.evaluate",
    "Runtime.releaseObject",
    "ScriptProfiler.startTracking",
    "ScriptProfiler.stopTracking",
    "Target.resume",
    "Target.sendMessageToTarget",
    "Worker.enable",
    "Worker.initialized",
    "Worker.sendMessageToWorker",
];
const REQUIRED_PROTOCOL_EVENTS: &[&str] = &[
    "Network.loadingFailed",
    "Network.loadingFinished",
    "Network.requestIntercepted",
    "Network.requestWillBeSent",
    "Page.frameNavigated",
    "Page.loadEventFired",
    "Playwright.pageProxyCreated",
    "Playwright.pageProxyDestroyed",
    "Playwright.provisionalLoadFailed",
    "ScriptProfiler.trackingComplete",
    "Target.didCommitProvisionalTarget",
    "Target.dispatchMessageFromTarget",
    "Target.targetCreated",
    "Target.targetDestroyed",
    "Worker.dispatchMessageFromWorker",
    "Worker.workerCreated",
    "Worker.workerTerminated",
];
const REQUIRED_PROTOCOL_PARAMETERS: &[(&str, &[&str])] = &[
    ("Network.requestIntercepted", &["requestId", "request"]),
    ("Page.loadEventFired", &["frameId"]),
    ("Page.overrideSetting", &["setting", "value"]),
    ("Page.overrideUserAgent", &["value"]),
    ("Page.setForcedColors", &["forcedColors"]),
    ("Page.setTouchEmulationEnabled", &["enabled"]),
    (
        "Playwright.setDownloadBehavior",
        &["behavior", "browserContextId", "downloadPath"],
    ),
    (
        "Runtime.awaitPromise",
        &["promiseObjectId", "returnByValue"],
    ),
    (
        "Runtime.evaluate",
        &["expression", "emulateUserGesture", "returnByValue"],
    ),
    ("Runtime.releaseObject", &["objectId"]),
    ("Target.sendMessageToTarget", &["message", "targetId"]),
    ("Worker.initialized", &["workerId"]),
    ("Worker.sendMessageToWorker", &["message", "workerId"]),
    ("Worker.dispatchMessageFromWorker", &["message", "workerId"]),
    ("Worker.workerCreated", &["url", "workerId"]),
    ("Worker.workerTerminated", &["workerId"]),
];

#[derive(Clone)]
pub(crate) struct WebKitAdapter {
    executable: PathBuf,
    revision: String,
    browser_version: String,
    playwright_version: String,
    executable_sha256: String,
    launch_arguments: Vec<String>,
}

impl EngineAdapter for WebKitAdapter {
    type Lane = WebKitLane;

    fn discover(installation: &RuntimeInstallation) -> Result<Self> {
        let webkit = installation.browser(BrowserName::Webkit)?;
        let browser_directory = webkit.directory().to_owned();
        validate_private_protocol(&browser_directory.join("protocol.json"))?;
        let executable = browser_directory.join(if cfg!(windows) {
            "Playwright.exe"
        } else {
            "pw_run.sh"
        });
        if !executable.is_file() {
            bail!(
                "Playwright WebKit revision {} is not installed at {}; run `npx playwright install webkit` for the pinned sidecar",
                webkit.revision(),
                executable.display()
            );
        }
        let executable = fs::canonicalize(&executable)
            .with_context(|| format!("failed to resolve {}", executable.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            if fs::metadata(&executable)?.permissions().mode() & 0o111 == 0 {
                bail!(
                    "Playwright WebKit launcher is not executable: {}",
                    executable.display()
                );
            }
        }
        let mut launch_arguments = vec!["--inspector-pipe".to_owned()];
        if cfg!(windows) {
            launch_arguments.push("--disable-accelerated-compositing".to_owned());
        }
        launch_arguments.push("--headless".to_owned());
        launch_arguments.push("--no-startup-window".to_owned());

        Ok(Self {
            executable_sha256: sha256_file(&executable)?,
            executable,
            revision: webkit.revision().to_owned(),
            browser_version: webkit.browser_version().to_owned(),
            playwright_version: installation.playwright_version().to_owned(),
            launch_arguments,
        })
    }

    fn launch(&self) -> Result<Self::Lane> {
        WebKitLane::launch(self)
    }
}

impl WebKitAdapter {
    fn adapter_evidence(&self) -> AdapterEvidence {
        AdapterEvidence::Webkit {
            playwright: self.playwright_version.clone(),
            webkit_revision: self.revision.clone(),
            executable_sha256: self.executable_sha256.clone(),
            protocol_version: ADAPTER_PROTOCOL_VERSION,
            browser_workload_version: BROWSER_WORKLOAD_VERSION,
        }
    }
}

#[derive(Deserialize)]
struct ProtocolDomain {
    domain: String,
    #[serde(default)]
    commands: Vec<ProtocolMember>,
    #[serde(default)]
    events: Vec<ProtocolMember>,
}

#[derive(Deserialize)]
struct ProtocolMember {
    name: String,
    #[serde(default)]
    parameters: Vec<ProtocolParameter>,
}

#[derive(Deserialize)]
struct ProtocolParameter {
    name: String,
}

fn validate_private_protocol(path: &Path) -> Result<()> {
    let domains: Vec<ProtocolDomain> = serde_json::from_slice(
        &fs::read(path)
            .with_context(|| format!("failed to read WebKit protocol {}", path.display()))?,
    )
    .with_context(|| format!("invalid WebKit protocol {}", path.display()))?;
    let mut commands = HashMap::<String, HashSet<String>>::new();
    let mut events = HashMap::<String, HashSet<String>>::new();
    for domain in domains {
        for command in domain.commands {
            commands.insert(
                format!("{}.{}", domain.domain, command.name),
                command
                    .parameters
                    .into_iter()
                    .map(|parameter| parameter.name)
                    .collect(),
            );
        }
        for event in domain.events {
            events.insert(
                format!("{}.{}", domain.domain, event.name),
                event
                    .parameters
                    .into_iter()
                    .map(|parameter| parameter.name)
                    .collect(),
            );
        }
    }
    let missing_commands = REQUIRED_PROTOCOL_COMMANDS
        .iter()
        .filter(|method| !commands.contains_key(**method))
        .copied()
        .collect::<Vec<_>>();
    let missing_events = REQUIRED_PROTOCOL_EVENTS
        .iter()
        .filter(|event| !events.contains_key(**event))
        .copied()
        .collect::<Vec<_>>();
    if !missing_commands.is_empty() || !missing_events.is_empty() {
        bail!(
            "pinned WebKit protocol is incompatible with Rust adapter version {ADAPTER_PROTOCOL_VERSION}; missing commands {missing_commands:?}, events {missing_events:?}"
        );
    }
    for (member, required) in REQUIRED_PROTOCOL_PARAMETERS {
        let available = commands
            .get(*member)
            .or_else(|| events.get(*member))
            .with_context(|| format!("WebKit protocol member {member} disappeared"))?;
        let missing = required
            .iter()
            .filter(|parameter| !available.contains(**parameter))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "pinned WebKit protocol is incompatible with Rust adapter version {ADAPTER_PROTOCOL_VERSION}; {member} is missing parameters {missing:?}"
            );
        }
    }
    Ok(())
}

pub(crate) struct WebKitLane {
    connection: InspectorConnection<BrowserProcess>,
    browser: BrowserEvidence,
    adapter: AdapterEvidence,
    closed: bool,
}

impl WebKitLane {
    fn launch(installation: &WebKitAdapter) -> Result<Self> {
        let process = BrowserProcess::spawn(
            "bperf-webkit-",
            "rust-webkit",
            &installation.executable,
            &installation.launch_arguments,
        )?;
        let download_directory = process.working_directory().join("downloads");
        fs::create_dir(&download_directory)
            .context("failed to create the isolated WebKit download directory")?;
        let root_pid = process.pid();
        let browser = BrowserEvidence {
            root_pid,
            executable_path: installation.executable.to_string_lossy().into_owned(),
            version: installation.browser_version.clone(),
            launch_args: installation.launch_arguments.clone(),
        };
        let mut connection = InspectorConnection::new(
            process,
            default_user_agent(&installation.browser_version),
            download_directory,
        );
        connection
            .send_browser("Playwright.enable", json!({}), REQUEST_TIMEOUT)
            .context("Playwright WebKit rejected Playwright.enable")?;
        Ok(Self {
            connection,
            browser,
            adapter: installation.adapter_evidence(),
            closed: false,
        })
    }

    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture> {
        let config = default_browser_config();
        let page = self.connection.open_page(&config)?;
        let result = (|| {
            let artifacts = CaptureArtifacts::prepare(Engine::Webkit, artifact_directory)?;
            let anchor = decode_runtime_anchor(
                self.connection
                    .evaluate(&page.page_proxy_id, RUNTIME_ANCHOR_EXPRESSION)?,
            )
            .context("WebKit runtime anchor capture failed")?;
            self.connection.clear_profile(&page.page_proxy_id)?;
            self.connection.send_current_target(
                &page.page_proxy_id,
                "ScriptProfiler.startTracking",
                json!({"includeSamples": true}),
                REQUEST_TIMEOUT,
            )?;
            self.connection
                .evaluate(&page.page_proxy_id, DOCTOR_PROBE_EXPRESSION)?;
            self.connection.send_current_target(
                &page.page_proxy_id,
                "ScriptProfiler.stopTracking",
                json!({}),
                REQUEST_TIMEOUT,
            )?;
            let profile = self
                .connection
                .wait_for_profile(&page.page_proxy_id, CAPTURE_TIMEOUT)?;
            if profile_traces(&profile).len() < 10 {
                bail!("WebKit CPU profile did not contain enough samples");
            }
            self.connection
                .evaluate(&page.page_proxy_id, SETTLE_EXPRESSION)?;
            self.connection.send_current_target(
                &page.page_proxy_id,
                "Heap.enable",
                json!({}),
                REQUEST_TIMEOUT,
            )?;
            let heap = self.connection.send_current_target(
                &page.page_proxy_id,
                "Heap.snapshot",
                json!({}),
                CAPTURE_TIMEOUT,
            )?;
            let snapshot = heap
                .get("snapshotData")
                .and_then(Value::as_str)
                .context("WebKit heap snapshot returned no data")?;
            parse_live_heap_bytes(snapshot)?;
            let artifacts = finish_capture_artifacts(artifacts, &profile, snapshot, None)?;
            Ok(ProbeCapture {
                adapter: self.adapter.clone(),
                browser: self.browser.clone(),
                anchor,
                artifacts,
            })
        })();
        combine_page_close(result, self.connection.close_page(page))
    }

    fn measure_trial(&mut self, request: AdapterTrialRequest<'_>) -> Result<TrialCapture> {
        let page = self.connection.open_page(request.browser)?;
        let result = (|| {
            self.connection
                .navigate(&page.page_proxy_id, request.target_url)?;
            self.connection.wait_for_expression(
                &page.page_proxy_id,
                WORKLOAD_READY_EXPRESSION,
                PAGE_READY_TIMEOUT,
            )?;
            let script = WorkloadScript::new(request.operations)?;
            self.connection
                .evaluate(&page.page_proxy_id, &script.prepare())?;
            let selected = self.connection.evaluate(
                &page.page_proxy_id,
                &script.select_batch_size(request.batches)?,
            )?;
            let batch_size =
                decode_batch_size(selected).context("WebKit batch calibration failed")?;

            self.connection.start_profile_capture(&page.page_proxy_id)?;
            let workload = decode_workload(
                self.connection
                    .evaluate(&page.page_proxy_id, &script.execute(batch_size))?,
            )
            .context("WebKit workload execution failed")?;
            let profiles = self.connection.stop_profile_capture(&page.page_proxy_id)?;
            let cpu_active_ms = profiles.iter().try_fold(0.0, |total, realm| {
                Ok::<_, anyhow::Error>(
                    total + benchmark_profile_cpu_milliseconds(&realm.profile, request.target_url)?,
                )
            })? / f64::from(batch_size);
            if !cpu_active_ms.is_finite() || cpu_active_ms <= 0.0 {
                bail!("WebKit CPU profiles have no positive benchmark sample duration");
            }

            self.connection
                .evaluate(&page.page_proxy_id, SETTLE_EXPRESSION)?;
            let mut heap_bytes = 0_u64;
            let mut artifact_evidence = Vec::with_capacity(profiles.len() * 3);
            for realm in profiles {
                let snapshot = self
                    .connection
                    .capture_realm_heap(&page.page_proxy_id, &realm.descriptor.realm)?;
                heap_bytes = heap_bytes
                    .checked_add(parse_live_heap_bytes(&snapshot)?)
                    .context("WebKit aggregate heap size overflowed")?;
                let artifacts = CaptureArtifacts::prepare_scope(
                    Engine::Webkit,
                    request.artifact_root,
                    &realm.descriptor.capture_scope,
                )?;
                artifact_evidence.extend(finish_capture_artifacts(
                    artifacts,
                    &realm.profile,
                    &snapshot,
                    Some(request.target_url),
                )?);
            }
            self.connection
                .finish_complete_capture(&page.page_proxy_id)?;
            Ok(TrialCapture {
                workload,
                cpu_active_ms,
                js_heap_live_bytes: heap_bytes,
                artifacts: artifact_evidence,
            })
        })();
        combine_page_close(result, self.connection.close_page(page))
    }

    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection> {
        let config = default_browser_config();
        let page = self.connection.open_page(&config)?;
        let result = (|| {
            self.connection.navigate(&page.page_proxy_id, target_url)?;
            self.connection.wait_for_expression(
                &page.page_proxy_id,
                BENCHMARK_READY_EXPRESSION,
                PAGE_READY_TIMEOUT,
            )?;
            let description = self
                .connection
                .evaluate(&page.page_proxy_id, BENCHMARK_DESCRIPTION_EXPRESSION)?;
            if description.is_null() {
                bail!("WebKit benchmark page returned no description");
            }
            let result = if let Some(case_id) = case_id {
                let script = WorkloadScript::new(&[json!({"case_id": case_id})])?;
                self.connection
                    .evaluate(&page.page_proxy_id, &script.inspect_result())?
            } else {
                Value::Null
            };
            Ok(BenchmarkInspection {
                description,
                result: case_id.map(|_| result),
            })
        })();
        combine_page_close(result, self.connection.close_page(page))
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.connection.close()
    }

    fn terminate(&mut self) -> Result<()> {
        self.closed = true;
        self.connection.terminate()
    }
}

impl EngineLane for WebKitLane {
    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture> {
        WebKitLane::probe(self, artifact_directory)
    }

    fn measure_trial(&mut self, request: AdapterTrialRequest<'_>) -> Result<TrialCapture> {
        WebKitLane::measure_trial(self, request)
    }

    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection> {
        WebKitLane::inspect_benchmark(self, target_url, case_id)
    }

    fn close(&mut self) -> Result<()> {
        WebKitLane::close(self)
    }

    fn terminate(&mut self) -> Result<()> {
        WebKitLane::terminate(self)
    }
}

impl Drop for WebKitLane {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.connection.terminate();
            self.closed = true;
        }
    }
}

fn combine_page_close<T>(result: Result<T>, close: Result<()>) -> Result<T> {
    match (result, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(close_error)) => Err(close_error.context("failed to close WebKit trial state")),
        (Err(error), Err(close_error)) => Err(error.context(format!(
            "WebKit capture also failed to close its isolated context: {close_error:#}"
        ))),
    }
}

fn default_user_agent(browser_version: &str) -> String {
    format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/605.1.15 (KHTML, like Gecko) Version/{browser_version} Safari/605.1.15"
    )
}

#[derive(Clone)]
struct PageRoute {
    browser_context_id: String,
    config: BrowserTrialConfig,
    current_target: Option<String>,
    provisional_target: Option<String>,
    targets: HashMap<String, TargetRoute>,
    capture_phase: WebKitCapturePhase,
    next_worker_scope: usize,
    proxy_initialized: bool,
}

#[derive(Clone, Default)]
struct TargetRoute {
    main_frame_id: Option<String>,
    load_fired: bool,
    destroyed: bool,
    request_urls: HashMap<String, String>,
    pending_interceptions: HashSet<String>,
    profile: Option<Value>,
    profiler_started: bool,
    workers: HashMap<String, WorkerRoute>,
}

#[derive(Clone)]
struct WorkerRoute {
    capture_scope: String,
    url: String,
    profile: Option<Value>,
    profiler_started: bool,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum WebKitCapturePhase {
    #[default]
    Idle,
    Profiling,
    Finalizing,
}

struct OpenPage {
    browser_context_id: String,
    page_proxy_id: String,
}

#[derive(Clone)]
enum WebKitRealm {
    Page {
        target_id: String,
    },
    Worker {
        target_id: String,
        worker_id: String,
    },
}

#[derive(Clone)]
struct WebKitRealmDescriptor {
    capture_scope: String,
    source_url: String,
    realm: WebKitRealm,
}

struct WebKitRealmProfile {
    descriptor: WebKitRealmDescriptor,
    profile: Value,
}

trait InspectorTransport {
    fn send(&mut self, message: &Value) -> Result<()>;
    fn receive(&self, timeout: Duration) -> Result<Value>;
    fn wait_for_exit(&mut self) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
}

impl InspectorTransport for BrowserProcess {
    fn send(&mut self, message: &Value) -> Result<()> {
        BrowserProcess::send(self, message)
    }

    fn receive(&self, timeout: Duration) -> Result<Value> {
        BrowserProcess::receive(self, timeout)
    }

    fn wait_for_exit(&mut self) -> Result<()> {
        BrowserProcess::wait_for_exit(self)
    }

    fn terminate(&mut self) -> Result<()> {
        BrowserProcess::terminate(self)
    }
}

struct InspectorConnection<Transport: InspectorTransport> {
    process: Transport,
    user_agent: String,
    download_directory: PathBuf,
    next_id: u64,
    responses: HashMap<u64, Result<Value, String>>,
    background_responses: HashMap<u64, String>,
    pages: HashMap<String, PageRoute>,
    closed_pages: HashSet<String>,
    contexts: HashMap<String, BrowserTrialConfig>,
    closing_contexts: HashSet<String>,
    pending_page_messages: HashMap<String, Vec<Value>>,
    fatal_error: Option<String>,
}

impl<Transport: InspectorTransport> InspectorConnection<Transport> {
    fn new(process: Transport, user_agent: String, download_directory: PathBuf) -> Self {
        Self {
            process,
            user_agent,
            download_directory,
            next_id: 1,
            responses: HashMap::new(),
            background_responses: HashMap::new(),
            pages: HashMap::new(),
            closed_pages: HashSet::new(),
            contexts: HashMap::new(),
            closing_contexts: HashSet::new(),
            pending_page_messages: HashMap::new(),
            fatal_error: None,
        }
    }

    fn send_browser(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.allocate_id();
        self.process.send(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.wait_for_response(id, timeout)
            .with_context(|| format!("WebKit browser command {method} failed"))
    }

    fn send_page_proxy(
        &mut self,
        page_proxy_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.allocate_id();
        self.process.send(&json!({
            "id": id,
            "method": method,
            "params": params,
            "pageProxyId": page_proxy_id,
        }))?;
        self.wait_for_response(id, timeout)
            .with_context(|| format!("WebKit page-proxy command {method} failed"))
    }

    fn send_target(
        &mut self,
        page_proxy_id: &str,
        target_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let nested_id = self.allocate_id();
        let outer_id = self.allocate_id();
        let message = serde_json::to_string(&json!({
            "id": nested_id,
            "method": method,
            "params": params,
        }))?;
        self.process.send(&json!({
            "id": outer_id,
            "method": "Target.sendMessageToTarget",
            "params": {
                "message": message,
                "targetId": target_id,
            },
            "pageProxyId": page_proxy_id,
        }))?;
        let deadline = Instant::now() + timeout;
        self.wait_for_response(outer_id, deadline.saturating_duration_since(Instant::now()))
            .with_context(|| format!("WebKit could not route {method} to target {target_id}"))?;
        self.wait_for_response(
            nested_id,
            deadline.saturating_duration_since(Instant::now()),
        )
        .with_context(|| format!("WebKit target command {method} failed"))
    }

    fn send_current_target(
        &mut self,
        page_proxy_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let target = self.current_target(page_proxy_id)?;
        self.send_target(page_proxy_id, &target, method, params, timeout)
    }

    fn send_worker(
        &mut self,
        page_proxy_id: &str,
        target_id: &str,
        worker_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.allocate_id();
        let message = serde_json::to_string(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;
        let deadline = Instant::now() + timeout;
        self.send_target(
            page_proxy_id,
            target_id,
            "Worker.sendMessageToWorker",
            json!({
                "workerId": worker_id,
                "message": message,
            }),
            deadline.saturating_duration_since(Instant::now()),
        )
        .with_context(|| format!("WebKit could not route {method} to worker {worker_id}"))?;
        self.wait_for_response(id, deadline.saturating_duration_since(Instant::now()))
            .with_context(|| format!("WebKit worker command {method} failed"))
    }

    fn send_target_without_waiting(
        &mut self,
        page_proxy_id: &str,
        target_id: &str,
        method: &str,
        params: Value,
    ) -> Result<()> {
        let nested_id = self.allocate_id();
        let outer_id = self.allocate_id();
        self.background_responses
            .insert(nested_id, method.to_owned());
        self.background_responses
            .insert(outer_id, format!("Target.sendMessageToTarget for {method}"));
        let message = serde_json::to_string(&json!({
            "id": nested_id,
            "method": method,
            "params": params,
        }))?;
        self.process.send(&json!({
            "id": outer_id,
            "method": "Target.sendMessageToTarget",
            "params": {
                "message": message,
                "targetId": target_id,
            },
            "pageProxyId": page_proxy_id,
        }))
    }

    fn wait_for_response(&mut self, id: u64, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            self.check_fatal_error()?;
            if let Some(response) = self.responses.remove(&id) {
                return response.map_err(anyhow::Error::msg);
            }
            self.pump_until(deadline)?;
        }
    }

    fn pump_until(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("WebKit inspector request timed out");
        }
        let message = self.process.receive(remaining)?;
        self.dispatch_message(message)
    }

    fn dispatch_message(&mut self, message: Value) -> Result<()> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            self.store_response(id, &message);
            return Ok(());
        }
        if let Some(page_proxy_id) = message.get("pageProxyId").and_then(Value::as_str) {
            let page_proxy_id = page_proxy_id.to_owned();
            if self.pages.contains_key(&page_proxy_id) {
                return self.dispatch_page_message(&page_proxy_id, message);
            }
            if self.closed_pages.contains(&page_proxy_id) {
                return Ok(());
            }
            self.pending_page_messages
                .entry(page_proxy_id)
                .or_default()
                .push(message);
            return Ok(());
        }
        self.dispatch_browser_event(message)
    }

    fn store_response(&mut self, id: u64, message: &Value) {
        if let Some(method) = self.background_responses.remove(&id) {
            if let Some(error) = message.get("error") {
                self.fatal_error
                    .get_or_insert_with(|| format!("{method} failed: {}", protocol_error(error)));
            }
            return;
        }
        let response = if let Some(error) = message.get("error") {
            Err(protocol_error(error))
        } else {
            Ok(message.get("result").cloned().unwrap_or(Value::Null))
        };
        self.responses.insert(id, response);
    }

    fn dispatch_browser_event(&mut self, message: Value) -> Result<()> {
        match message.get("method").and_then(Value::as_str) {
            Some("Playwright.pageProxyCreated") => {
                let params = message
                    .get("params")
                    .context("pageProxyCreated has no params")?;
                let page_proxy_id = required_string(params, "pageProxyId")?;
                let context_id = required_string(params, "browserContextId")?;
                self.closed_pages.remove(&page_proxy_id);
                if let Some(config) = self.contexts.get(&context_id).cloned() {
                    self.pages
                        .entry(page_proxy_id.clone())
                        .or_insert(PageRoute {
                            browser_context_id: context_id,
                            config,
                            current_target: None,
                            provisional_target: None,
                            targets: HashMap::new(),
                            capture_phase: WebKitCapturePhase::Idle,
                            next_worker_scope: 1,
                            proxy_initialized: false,
                        });
                    self.replay_pending_page_messages(&page_proxy_id)?;
                }
            }
            Some("Playwright.pageProxyDestroyed") => {
                if let Some(page_proxy_id) = message
                    .get("params")
                    .and_then(|params| params.get("pageProxyId"))
                    .and_then(Value::as_str)
                {
                    let expected = self.pages.get(page_proxy_id).is_some_and(|page| {
                        self.closing_contexts.contains(&page.browser_context_id)
                    });
                    if self.pages.remove(page_proxy_id).is_some() && !expected {
                        self.fatal_error =
                            Some(format!("WebKit page proxy {page_proxy_id} was destroyed"));
                    }
                }
                if let Some(page_proxy_id) = message
                    .get("params")
                    .and_then(|params| params.get("pageProxyId"))
                    .and_then(Value::as_str)
                {
                    self.closed_pages.insert(page_proxy_id.to_owned());
                    self.pending_page_messages.remove(page_proxy_id);
                }
            }
            Some("Playwright.provisionalLoadFailed") => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                self.fatal_error = Some(format!(
                    "WebKit provisional navigation failed: {}",
                    params
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                ));
            }
            Some(_) | None => {}
        }
        Ok(())
    }

    fn replay_pending_page_messages(&mut self, page_proxy_id: &str) -> Result<()> {
        let pending = self
            .pending_page_messages
            .remove(page_proxy_id)
            .unwrap_or_default();
        for message in pending {
            self.dispatch_page_message(page_proxy_id, message)?;
        }
        Ok(())
    }

    fn dispatch_page_message(&mut self, page_proxy_id: &str, message: Value) -> Result<()> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            self.store_response(id, &message);
            return Ok(());
        }
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        match method {
            Some("Target.targetCreated") => {
                let info = params
                    .get("targetInfo")
                    .context("Target.targetCreated has no targetInfo")?;
                let target_type = required_string(info, "type")?;
                if target_type != "page" {
                    bail!(
                        "WebKit exposed a separate {target_type} target that its complete capture contract does not support"
                    );
                }
                let target_id = required_string(info, "targetId")?;
                let provisional = info
                    .get("isProvisional")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let paused = info
                    .get("isPaused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                {
                    let page = self
                        .pages
                        .get_mut(page_proxy_id)
                        .context("target belongs to an unknown WebKit page")?;
                    page.targets
                        .entry(target_id.clone())
                        .or_insert_with(TargetRoute::default);
                    if provisional {
                        page.provisional_target = Some(target_id.clone());
                    } else if page.current_target.is_none() {
                        page.current_target = Some(target_id.clone());
                    }
                }
                self.initialize_target(page_proxy_id, &target_id)?;
                if paused {
                    self.send_page_proxy(
                        page_proxy_id,
                        "Target.resume",
                        json!({"targetId": target_id}),
                        REQUEST_TIMEOUT,
                    )?;
                }
            }
            Some("Target.didCommitProvisionalTarget") => {
                let new_target = required_string(&params, "newTargetId")?;
                let old_target = required_string(&params, "oldTargetId")?;
                let page = self
                    .pages
                    .get_mut(page_proxy_id)
                    .context("provisional commit belongs to an unknown page")?;
                if page.current_target.as_deref() != Some(old_target.as_str())
                    || page.provisional_target.as_deref() != Some(new_target.as_str())
                {
                    bail!("WebKit committed an unknown provisional target");
                }
                page.current_target = Some(new_target);
                page.provisional_target = None;
            }
            Some("Target.targetDestroyed") => {
                let target_id = required_string(&params, "targetId")?;
                let crashed = params
                    .get("crashed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let page_is_closing = self
                    .pages
                    .get(page_proxy_id)
                    .is_some_and(|page| self.closing_contexts.contains(&page.browser_context_id));
                if let Some(page) = self.pages.get_mut(page_proxy_id) {
                    if let Some(target) = page.targets.get_mut(&target_id) {
                        target.destroyed = true;
                    }
                    if page.current_target.as_deref() == Some(target_id.as_str())
                        && !page_is_closing
                    {
                        self.fatal_error = Some(if crashed {
                            "WebKit page target crashed".to_owned()
                        } else {
                            "WebKit page target was destroyed".to_owned()
                        });
                    }
                }
            }
            Some("Target.dispatchMessageFromTarget") => {
                let target_id = required_string(&params, "targetId")?;
                let nested: Value = serde_json::from_str(
                    params
                        .get("message")
                        .and_then(Value::as_str)
                        .context("target dispatch has no nested message")?,
                )
                .context("WebKit target dispatched invalid JSON")?;
                if let Some(id) = nested.get("id").and_then(Value::as_u64) {
                    self.store_response(id, &nested);
                } else {
                    self.dispatch_target_event(page_proxy_id, &target_id, nested)?;
                }
            }
            Some(_) | None => {}
        }
        Ok(())
    }

    fn dispatch_target_event(
        &mut self,
        page_proxy_id: &str,
        target_id: &str,
        message: Value,
    ) -> Result<()> {
        let method = message.get("method").and_then(Value::as_str);
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        match method {
            Some("Page.loadEventFired") => {
                if let Some(target) = self
                    .pages
                    .get_mut(page_proxy_id)
                    .and_then(|page| page.targets.get_mut(target_id))
                {
                    let frame_id = required_string(&params, "frameId")?;
                    if target.main_frame_id.as_deref() == Some(frame_id.as_str()) {
                        target.load_fired = true;
                    }
                }
            }
            Some("Page.frameNavigated") => {
                if let Some(frame_id) = params
                    .get("frame")
                    .and_then(|frame| frame.get("id"))
                    .and_then(Value::as_str)
                    && let Some(target) = self
                        .pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                {
                    target
                        .main_frame_id
                        .get_or_insert_with(|| frame_id.to_owned());
                }
            }
            Some("Network.requestIntercepted") => {
                let request_id = required_string(&params, "requestId")?;
                let inline_url = params
                    .get("url")
                    .or_else(|| params.get("request").and_then(|request| request.get("url")))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let url = inline_url.or_else(|| {
                    self.pages
                        .get(page_proxy_id)
                        .and_then(|page| page.targets.get(target_id))
                        .and_then(|target| target.request_urls.get(&request_id))
                        .cloned()
                });
                if let Some(url) = url {
                    self.resolve_interception(page_proxy_id, target_id, &request_id, &url)?;
                } else {
                    self.pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                        .context("interception belongs to an unknown WebKit target")?
                        .pending_interceptions
                        .insert(request_id);
                }
            }
            Some("Network.requestWillBeSent") => {
                let request_id = required_string(&params, "requestId")?;
                let url = params
                    .get("request")
                    .and_then(|request| request.get("url"))
                    .and_then(Value::as_str)
                    .context("WebKit request event has no URL")?
                    .to_owned();
                let pending = {
                    let target = self
                        .pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                        .context("request belongs to an unknown WebKit target")?;
                    target.request_urls.insert(request_id.clone(), url.clone());
                    target.pending_interceptions.remove(&request_id)
                };
                if pending {
                    self.resolve_interception(page_proxy_id, target_id, &request_id, &url)?;
                }
            }
            Some("Network.loadingFinished") | Some("Network.loadingFailed") => {
                if let Some(request_id) = params.get("requestId").and_then(Value::as_str)
                    && let Some(target) = self
                        .pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                {
                    target.request_urls.remove(request_id);
                    target.pending_interceptions.remove(request_id);
                }
            }
            Some("Worker.workerCreated") => {
                let worker_id = required_string(&params, "workerId")?;
                let url = required_string(&params, "url")?;
                let (capture_scope, capture_phase) = {
                    let page = self
                        .pages
                        .get_mut(page_proxy_id)
                        .context("worker belongs to an unknown WebKit page")?;
                    let capture_scope = format!("worker-{}", page.next_worker_scope);
                    page.next_worker_scope = page
                        .next_worker_scope
                        .checked_add(1)
                        .context("WebKit worker capture-scope counter overflowed")?;
                    let replaced = page
                        .targets
                        .get_mut(target_id)
                        .context("worker belongs to an unknown WebKit target")?
                        .workers
                        .insert(
                            worker_id.clone(),
                            WorkerRoute {
                                capture_scope: capture_scope.clone(),
                                url,
                                profile: None,
                                profiler_started: false,
                            },
                        );
                    if replaced.is_some() {
                        bail!("WebKit reported duplicate worker {worker_id}");
                    }
                    (capture_scope, page.capture_phase)
                };
                self.initialize_worker(
                    page_proxy_id,
                    target_id,
                    &worker_id,
                    &capture_scope,
                    capture_phase,
                )?;
            }
            Some("Worker.dispatchMessageFromWorker") => {
                let worker_id = required_string(&params, "workerId")?;
                let nested: Value = serde_json::from_str(
                    params
                        .get("message")
                        .and_then(Value::as_str)
                        .context("worker dispatch has no nested message")?,
                )
                .context("WebKit worker dispatched invalid JSON")?;
                if let Some(id) = nested.get("id").and_then(Value::as_u64) {
                    self.store_response(id, &nested);
                } else if nested.get("method").and_then(Value::as_str)
                    == Some("ScriptProfiler.trackingComplete")
                {
                    let profile = nested.get("params").cloned().unwrap_or_else(|| json!({}));
                    self.pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                        .and_then(|target| target.workers.get_mut(&worker_id))
                        .context("profile belongs to an unknown WebKit worker")?
                        .profile = Some(profile);
                }
            }
            Some("Worker.workerTerminated") => {
                let worker_id = required_string(&params, "workerId")?;
                let page = self
                    .pages
                    .get_mut(page_proxy_id)
                    .context("terminated worker belongs to an unknown WebKit page")?;
                let worker = page
                    .targets
                    .get_mut(target_id)
                    .and_then(|target| target.workers.remove(&worker_id));
                if page.capture_phase != WebKitCapturePhase::Idle {
                    self.fatal_error = Some(if worker.is_some() {
                        "WebKit terminated a worker during complete capture".to_owned()
                    } else {
                        "WebKit terminated an unknown worker during complete capture".to_owned()
                    });
                }
            }
            Some("ScriptProfiler.trackingComplete") => {
                self.pages
                    .get_mut(page_proxy_id)
                    .and_then(|page| page.targets.get_mut(target_id))
                    .context("profile belongs to an unknown WebKit target")?
                    .profile = Some(params);
            }
            Some(_) | None => {}
        }
        Ok(())
    }

    fn initialize_worker(
        &mut self,
        page_proxy_id: &str,
        target_id: &str,
        worker_id: &str,
        capture_scope: &str,
        capture_phase: WebKitCapturePhase,
    ) -> Result<()> {
        self.send_worker(
            page_proxy_id,
            target_id,
            worker_id,
            "Runtime.enable",
            json!({}),
            REQUEST_TIMEOUT,
        )?;
        let installed = self.send_worker(
            page_proxy_id,
            target_id,
            worker_id,
            "Runtime.evaluate",
            json!({
                "expression": bootstrap_source(),
                "returnByValue": true,
                "doNotPauseOnExceptionsAndMuteConsole": false,
            }),
            REQUEST_TIMEOUT,
        )?;
        if installed
            .get("wasThrown")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("WebKit rejected the browser workload bootstrap in {capture_scope}");
        }
        match capture_phase {
            WebKitCapturePhase::Idle => {}
            WebKitCapturePhase::Profiling => {
                self.start_worker_profiler(page_proxy_id, target_id, worker_id)?;
            }
            WebKitCapturePhase::Finalizing => {
                bail!("WebKit created a worker after profile finalization began");
            }
        }
        self.send_target(
            page_proxy_id,
            target_id,
            "Worker.initialized",
            json!({"workerId": worker_id}),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    fn resolve_interception(
        &mut self,
        page_proxy_id: &str,
        target_id: &str,
        request_id: &str,
        url: &str,
    ) -> Result<()> {
        let (method, params) = if is_allowed_trial_url(url) {
            (
                "Network.interceptWithRequest",
                json!({"requestId": request_id}),
            )
        } else {
            (
                "Network.interceptRequestWithError",
                json!({"requestId": request_id, "errorType": "Cancellation"}),
            )
        };
        self.send_target_without_waiting(page_proxy_id, target_id, method, params)
    }

    fn initialize_target(&mut self, page_proxy_id: &str, target_id: &str) -> Result<()> {
        let (config, initialize_proxy, user_agent) = {
            let page = self
                .pages
                .get_mut(page_proxy_id)
                .context("cannot initialize an unknown WebKit page")?;
            let initialize_proxy = !page.proxy_initialized;
            if initialize_proxy {
                page.proxy_initialized = true;
            }
            (
                page.config.clone(),
                initialize_proxy,
                self.user_agent.clone(),
            )
        };
        if initialize_proxy {
            self.send_page_proxy(page_proxy_id, "Dialog.enable", json!({}), REQUEST_TIMEOUT)?;
            self.send_page_proxy(
                page_proxy_id,
                "Emulation.setActiveAndFocused",
                json!({"active": true}),
                REQUEST_TIMEOUT,
            )?;
            self.send_page_proxy(
                page_proxy_id,
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": config.viewport.width,
                    "height": config.viewport.height,
                    "fixedLayout": false,
                    "deviceScaleFactor": 1,
                }),
                REQUEST_TIMEOUT,
            )?;
        }
        self.send_target(
            page_proxy_id,
            target_id,
            "Page.enable",
            json!({}),
            REQUEST_TIMEOUT,
        )?;
        let tree = self.send_target(
            page_proxy_id,
            target_id,
            "Page.getResourceTree",
            json!({}),
            REQUEST_TIMEOUT,
        )?;
        let frame_id = tree
            .pointer("/frameTree/frame/id")
            .and_then(Value::as_str)
            .context("WebKit resource tree has no main frame")?
            .to_owned();
        self.pages
            .get_mut(page_proxy_id)
            .and_then(|page| page.targets.get_mut(target_id))
            .context("initialized WebKit target disappeared")?
            .main_frame_id = Some(frame_id);
        let bootstrap = bootstrap_source();
        for (method, params) in [
            ("Runtime.enable", json!({})),
            ("Worker.enable", json!({})),
            ("Page.overrideUserAgent", json!({"value": user_agent})),
            ("Network.enable", json!({})),
            ("Network.setInterceptionEnabled", json!({"enabled": true})),
            (
                "Network.setResourceCachingDisabled",
                json!({"disabled": true}),
            ),
            (
                "Network.addInterception",
                json!({"url": ".*", "stage": "request", "isRegex": true}),
            ),
            (
                "Network.setExtraHTTPHeaders",
                json!({"headers": {"Accept-Language": config.locale}}),
            ),
            ("Page.setTimeZone", json!({"timeZone": config.timezone_id})),
            ("Page.setEmulatedMedia", json!({"media": ""})),
            (
                "Page.overrideUserPreference",
                json!({"name": "PrefersReducedMotion", "value": "NoPreference"}),
            ),
            ("Page.setForcedColors", json!({"forcedColors": "None"})),
            (
                "Page.overrideUserPreference",
                json!({"name": "PrefersContrast", "value": "NoPreference"}),
            ),
            ("Page.setTouchEmulationEnabled", json!({"enabled": false})),
            (
                "Page.overrideSetting",
                json!({"setting": "DeviceOrientationEventEnabled", "value": false}),
            ),
            (
                "Page.overrideSetting",
                json!({"setting": "FullScreenEnabled", "value": true}),
            ),
            (
                "Page.overrideSetting",
                json!({"setting": "NotificationsEnabled", "value": true}),
            ),
            (
                "Page.overrideSetting",
                json!({"setting": "PointerLockEnabled", "value": true}),
            ),
            (
                "Page.overrideSetting",
                json!({"setting": "InputTypeMonthEnabled", "value": false}),
            ),
            (
                "Page.overrideSetting",
                json!({"setting": "InputTypeWeekEnabled", "value": false}),
            ),
            (
                "Page.overrideSetting",
                json!({"setting": "FixedBackgroundsPaintRelativeToDocument", "value": false}),
            ),
            (
                "Page.setScreenSizeOverride",
                json!({
                    "width": config.viewport.width,
                    "height": config.viewport.height,
                }),
            ),
            (
                "Page.setBootstrapScript",
                json!({"source": bootstrap.clone()}),
            ),
        ] {
            self.send_target(page_proxy_id, target_id, method, params, REQUEST_TIMEOUT)?;
        }
        let mut preference = Map::from_iter([(
            "name".to_owned(),
            Value::String("PrefersColorScheme".to_owned()),
        )]);
        if config.color_scheme != "no-preference" {
            preference.insert(
                "value".to_owned(),
                Value::String(if config.color_scheme == "dark" {
                    "Dark".to_owned()
                } else {
                    "Light".to_owned()
                }),
            );
        }
        self.send_target(
            page_proxy_id,
            target_id,
            "Page.overrideUserPreference",
            Value::Object(preference),
            REQUEST_TIMEOUT,
        )?;
        let installed = self.send_target(
            page_proxy_id,
            target_id,
            "Runtime.evaluate",
            json!({
                "expression": bootstrap,
                "returnByValue": true,
                "doNotPauseOnExceptionsAndMuteConsole": false,
            }),
            REQUEST_TIMEOUT,
        )?;
        if installed
            .get("wasThrown")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            bail!("WebKit rejected the browser workload bootstrap");
        }
        Ok(())
    }

    fn open_page(&mut self, config: &BrowserTrialConfig) -> Result<OpenPage> {
        let context = self.send_browser("Playwright.createContext", json!({}), REQUEST_TIMEOUT)?;
        let browser_context_id = required_string(&context, "browserContextId")?;
        self.contexts
            .insert(browser_context_id.clone(), config.clone());
        self.send_browser(
            "Playwright.setDownloadBehavior",
            json!({
                "behavior": "deny",
                "browserContextId": browser_context_id,
                "downloadPath": self.download_directory.to_string_lossy(),
            }),
            REQUEST_TIMEOUT,
        )?;
        self.send_browser(
            "Playwright.setLanguages",
            json!({
                "browserContextId": browser_context_id,
                "languages": [config.locale],
            }),
            REQUEST_TIMEOUT,
        )?;
        let page = self.send_browser(
            "Playwright.createPage",
            json!({"browserContextId": browser_context_id}),
            REQUEST_TIMEOUT,
        )?;
        let page_proxy_id = required_string(&page, "pageProxyId")?;
        if !self.pages.contains_key(&page_proxy_id) {
            self.pages.insert(
                page_proxy_id.clone(),
                PageRoute {
                    browser_context_id: browser_context_id.clone(),
                    config: config.clone(),
                    current_target: None,
                    provisional_target: None,
                    targets: HashMap::new(),
                    capture_phase: WebKitCapturePhase::Idle,
                    next_worker_scope: 1,
                    proxy_initialized: false,
                },
            );
            self.replay_pending_page_messages(&page_proxy_id)?;
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        while self
            .pages
            .get(&page_proxy_id)
            .and_then(|page| page.current_target.as_ref())
            .is_none()
        {
            self.check_fatal_error()?;
            self.pump_until(deadline)?;
        }
        Ok(OpenPage {
            browser_context_id,
            page_proxy_id,
        })
    }

    fn navigate(&mut self, page_proxy_id: &str, target_url: &str) -> Result<()> {
        if !is_allowed_adapter_url(target_url) {
            bail!("variant adapter returned a non-loopback URL: {target_url}");
        }
        let target_id = self.current_target(page_proxy_id)?;
        let frame_id = self
            .pages
            .get(page_proxy_id)
            .and_then(|page| page.targets.get(&target_id))
            .and_then(|target| target.main_frame_id.clone())
            .context("WebKit main frame was unavailable before navigation")?;
        for target in self
            .pages
            .get_mut(page_proxy_id)
            .context("cannot navigate an unknown WebKit page")?
            .targets
            .values_mut()
        {
            target.load_fired = false;
        }
        self.send_browser(
            "Playwright.navigate",
            json!({
                "url": target_url,
                "pageProxyId": page_proxy_id,
                "frameId": frame_id,
            }),
            REQUEST_TIMEOUT,
        )?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let current = self.current_target(page_proxy_id)?;
            let loaded = self
                .pages
                .get(page_proxy_id)
                .and_then(|page| page.targets.get(&current))
                .is_some_and(|target| target.load_fired);
            if loaded {
                break;
            }
            self.check_fatal_error()?;
            self.pump_until(deadline)?;
        }
        self.wait_for_expression(page_proxy_id, &installed_expression(), PAGE_READY_TIMEOUT)
    }

    fn evaluate(&mut self, page_proxy_id: &str, expression: &str) -> Result<Value> {
        self.evaluate_with_timeout(page_proxy_id, expression, CAPTURE_TIMEOUT)
    }

    fn evaluate_with_timeout(
        &mut self,
        page_proxy_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        let target_id = self.current_target(page_proxy_id)?;
        let evaluated = self.send_target(
            page_proxy_id,
            &target_id,
            "Runtime.evaluate",
            json!({
                "expression": format!("(async () => ({expression}))()"),
                "returnByValue": false,
                "generatePreview": false,
                "doNotPauseOnExceptionsAndMuteConsole": false,
                "emulateUserGesture": true,
            }),
            deadline.saturating_duration_since(Instant::now()),
        )?;
        if evaluated
            .get("wasThrown")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let remote = evaluated.get("result").cloned().unwrap_or(Value::Null);
            bail!(
                "WebKit page evaluation failed: {}",
                remote
                    .get("description")
                    .and_then(Value::as_str)
                    .or_else(|| remote.get("value").and_then(Value::as_str))
                    .unwrap_or("JavaScript exception")
            );
        }
        let promise_id = evaluated
            .get("result")
            .and_then(|result| result.get("objectId"))
            .and_then(Value::as_str)
            .context("WebKit evaluation returned no promise object")?
            .to_owned();
        let awaited = self.send_target(
            page_proxy_id,
            &target_id,
            "Runtime.awaitPromise",
            json!({
                "promiseObjectId": promise_id,
                "returnByValue": true,
                "generatePreview": false,
            }),
            deadline.saturating_duration_since(Instant::now()),
        )?;
        self.send_target_without_waiting(
            page_proxy_id,
            &target_id,
            "Runtime.releaseObject",
            json!({"objectId": promise_id}),
        )?;
        if awaited
            .get("wasThrown")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let remote = awaited.get("result").cloned().unwrap_or(Value::Null);
            bail!(
                "WebKit page evaluation failed: {}",
                remote
                    .get("description")
                    .and_then(Value::as_str)
                    .or_else(|| remote.get("value").and_then(Value::as_str))
                    .unwrap_or("JavaScript exception")
            );
        }
        let remote = awaited
            .get("result")
            .context("WebKit awaited evaluation returned no remote object")?;
        Ok(remote.get("value").cloned().unwrap_or(Value::Null))
    }

    fn wait_for_expression(
        &mut self,
        page_proxy_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("WebKit benchmark page did not become ready within {timeout:?}");
            }
            if self
                .evaluate_with_timeout(page_proxy_id, expression, remaining)?
                .as_bool()
                .unwrap_or(false)
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn clear_profile(&mut self, page_proxy_id: &str) -> Result<()> {
        let target_id = self.current_target(page_proxy_id)?;
        self.pages
            .get_mut(page_proxy_id)
            .and_then(|page| page.targets.get_mut(&target_id))
            .context("cannot profile an unknown WebKit target")?
            .profile = None;
        Ok(())
    }

    fn wait_for_profile(&mut self, page_proxy_id: &str, timeout: Duration) -> Result<Value> {
        let target_id = self.current_target(page_proxy_id)?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(profile) = self
                .pages
                .get_mut(page_proxy_id)
                .and_then(|page| page.targets.get_mut(&target_id))
                .and_then(|target| target.profile.take())
            {
                return Ok(profile);
            }
            self.check_fatal_error()?;
            self.pump_until(deadline)?;
        }
    }

    fn capture_realms(&self, page_proxy_id: &str) -> Result<Vec<WebKitRealmDescriptor>> {
        let page = self
            .pages
            .get(page_proxy_id)
            .context("cannot enumerate realms for an unknown WebKit page")?;
        let target_id = page
            .current_target
            .clone()
            .context("WebKit page has no current target")?;
        let target = page
            .targets
            .get(&target_id)
            .context("WebKit current target disappeared")?;
        let mut realms = vec![WebKitRealmDescriptor {
            capture_scope: "page".to_owned(),
            source_url: String::new(),
            realm: WebKitRealm::Page {
                target_id: target_id.clone(),
            },
        }];
        realms.extend(
            target
                .workers
                .iter()
                .map(|(worker_id, worker)| WebKitRealmDescriptor {
                    capture_scope: worker.capture_scope.clone(),
                    source_url: worker.url.clone(),
                    realm: WebKitRealm::Worker {
                        target_id: target_id.clone(),
                        worker_id: worker_id.clone(),
                    },
                }),
        );
        realms.sort_by(|left, right| left.capture_scope.cmp(&right.capture_scope));
        Ok(realms)
    }

    fn start_profile_capture(&mut self, page_proxy_id: &str) -> Result<()> {
        let page = self
            .pages
            .get_mut(page_proxy_id)
            .context("cannot profile an unknown WebKit page")?;
        if page.capture_phase != WebKitCapturePhase::Idle {
            bail!("WebKit profile capture is already active");
        }
        page.capture_phase = WebKitCapturePhase::Profiling;
        let realms = self.capture_realms(page_proxy_id)?;
        for descriptor in realms {
            match descriptor.realm {
                WebKitRealm::Page { target_id } => {
                    self.pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(&target_id))
                        .context("WebKit page target disappeared before profiling")?
                        .profile = None;
                    self.send_target(
                        page_proxy_id,
                        &target_id,
                        "ScriptProfiler.startTracking",
                        json!({"includeSamples": true}),
                        REQUEST_TIMEOUT,
                    )?;
                    self.pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(&target_id))
                        .context("WebKit page target disappeared while profiling began")?
                        .profiler_started = true;
                }
                WebKitRealm::Worker {
                    target_id,
                    worker_id,
                } => {
                    self.pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(&target_id))
                        .and_then(|target| target.workers.get_mut(&worker_id))
                        .context("WebKit worker disappeared before profiling")?
                        .profile = None;
                    if !self
                        .pages
                        .get(page_proxy_id)
                        .and_then(|page| page.targets.get(&target_id))
                        .and_then(|target| target.workers.get(&worker_id))
                        .is_some_and(|worker| worker.profiler_started)
                    {
                        self.start_worker_profiler(page_proxy_id, &target_id, &worker_id)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn start_worker_profiler(
        &mut self,
        page_proxy_id: &str,
        target_id: &str,
        worker_id: &str,
    ) -> Result<()> {
        self.send_worker(
            page_proxy_id,
            target_id,
            worker_id,
            "ScriptProfiler.startTracking",
            json!({"includeSamples": true}),
            REQUEST_TIMEOUT,
        )?;
        self.pages
            .get_mut(page_proxy_id)
            .and_then(|page| page.targets.get_mut(target_id))
            .and_then(|target| target.workers.get_mut(worker_id))
            .context("WebKit worker disappeared while profiling began")?
            .profiler_started = true;
        Ok(())
    }

    fn stop_profile_capture(&mut self, page_proxy_id: &str) -> Result<Vec<WebKitRealmProfile>> {
        let page = self
            .pages
            .get_mut(page_proxy_id)
            .context("cannot stop profiling an unknown WebKit page")?;
        if page.capture_phase != WebKitCapturePhase::Profiling {
            bail!("WebKit profile capture is not active");
        }
        page.capture_phase = WebKitCapturePhase::Finalizing;
        let realms = self.capture_realms(page_proxy_id)?;
        for descriptor in &realms {
            match &descriptor.realm {
                WebKitRealm::Page { target_id } => {
                    let started = self
                        .pages
                        .get(page_proxy_id)
                        .and_then(|page| page.targets.get(target_id))
                        .is_some_and(|target| target.profiler_started);
                    if !started {
                        bail!("WebKit page was not included from the start of profile capture");
                    }
                    self.send_target(
                        page_proxy_id,
                        target_id,
                        "ScriptProfiler.stopTracking",
                        json!({}),
                        REQUEST_TIMEOUT,
                    )?;
                    self.pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                        .context("WebKit page disappeared during profile finalization")?
                        .profiler_started = false;
                }
                WebKitRealm::Worker {
                    target_id,
                    worker_id,
                } => {
                    let started = self
                        .pages
                        .get(page_proxy_id)
                        .and_then(|page| page.targets.get(target_id))
                        .and_then(|target| target.workers.get(worker_id))
                        .is_some_and(|worker| worker.profiler_started);
                    if !started {
                        bail!(
                            "WebKit {} ({}) was not included from the start of profile capture",
                            descriptor.capture_scope,
                            descriptor.source_url
                        );
                    }
                    self.send_worker(
                        page_proxy_id,
                        target_id,
                        worker_id,
                        "ScriptProfiler.stopTracking",
                        json!({}),
                        REQUEST_TIMEOUT,
                    )?;
                    self.pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                        .and_then(|target| target.workers.get_mut(worker_id))
                        .context("WebKit worker disappeared during profile finalization")?
                        .profiler_started = false;
                }
            }
        }

        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        let mut profiles = Vec::with_capacity(realms.len());
        for descriptor in realms {
            let profile = loop {
                let profile = match &descriptor.realm {
                    WebKitRealm::Page { target_id } => self
                        .pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                        .and_then(|target| target.profile.take()),
                    WebKitRealm::Worker {
                        target_id,
                        worker_id,
                    } => self
                        .pages
                        .get_mut(page_proxy_id)
                        .and_then(|page| page.targets.get_mut(target_id))
                        .and_then(|target| target.workers.get_mut(worker_id))
                        .and_then(|worker| worker.profile.take()),
                };
                if let Some(profile) = profile {
                    break profile;
                }
                self.check_fatal_error()?;
                self.pump_until(deadline)?;
            };
            parse_profile(&profile).with_context(|| {
                format!(
                    "WebKit {} CPU capture was invalid",
                    descriptor.capture_scope
                )
            })?;
            profiles.push(WebKitRealmProfile {
                descriptor,
                profile,
            });
        }
        Ok(profiles)
    }

    fn capture_realm_heap(&mut self, page_proxy_id: &str, realm: &WebKitRealm) -> Result<String> {
        let heap = match realm {
            WebKitRealm::Page { target_id } => {
                self.send_target(
                    page_proxy_id,
                    target_id,
                    "Heap.enable",
                    json!({}),
                    REQUEST_TIMEOUT,
                )?;
                self.send_target(
                    page_proxy_id,
                    target_id,
                    "Heap.snapshot",
                    json!({}),
                    CAPTURE_TIMEOUT,
                )?
            }
            WebKitRealm::Worker {
                target_id,
                worker_id,
            } => {
                self.send_worker(
                    page_proxy_id,
                    target_id,
                    worker_id,
                    "Heap.enable",
                    json!({}),
                    REQUEST_TIMEOUT,
                )?;
                self.send_worker(
                    page_proxy_id,
                    target_id,
                    worker_id,
                    "Heap.snapshot",
                    json!({}),
                    CAPTURE_TIMEOUT,
                )?
            }
        };
        heap.get("snapshotData")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("WebKit heap snapshot returned no data")
    }

    fn finish_complete_capture(&mut self, page_proxy_id: &str) -> Result<()> {
        let page = self
            .pages
            .get_mut(page_proxy_id)
            .context("cannot finish capture for an unknown WebKit page")?;
        if page.capture_phase != WebKitCapturePhase::Finalizing {
            bail!("WebKit complete capture was not being finalized");
        }
        page.capture_phase = WebKitCapturePhase::Idle;
        self.check_fatal_error()
    }

    fn close_page(&mut self, page: OpenPage) -> Result<()> {
        self.closing_contexts
            .insert(page.browser_context_id.clone());
        let mut context_pages = self
            .pages
            .iter()
            .filter(|(_, route)| route.browser_context_id == page.browser_context_id)
            .map(|(page_proxy_id, _)| page_proxy_id.clone())
            .collect::<HashSet<_>>();
        let result = self.send_browser(
            "Playwright.deleteContext",
            json!({"browserContextId": page.browser_context_id}),
            REQUEST_TIMEOUT,
        );
        context_pages.extend(
            self.pages
                .iter()
                .filter(|(_, route)| route.browser_context_id == page.browser_context_id)
                .map(|(page_proxy_id, _)| page_proxy_id.clone()),
        );
        self.contexts.remove(&page.browser_context_id);
        self.closing_contexts.remove(&page.browser_context_id);
        for page_proxy_id in context_pages {
            self.pages.remove(&page_proxy_id);
            self.closed_pages.insert(page_proxy_id.clone());
            self.pending_page_messages.remove(&page_proxy_id);
        }
        result.map(|_| ())
    }

    fn current_target(&self, page_proxy_id: &str) -> Result<String> {
        self.pages
            .get(page_proxy_id)
            .and_then(|page| page.current_target.clone())
            .with_context(|| format!("WebKit page {page_proxy_id} has no current target"))
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn check_fatal_error(&mut self) -> Result<()> {
        if let Some(error) = self.fatal_error.take() {
            bail!("{error}");
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.process.send(&json!({
            "id": -9999,
            "method": "Playwright.close",
            "params": {},
        }))?;
        self.process.wait_for_exit()
    }

    fn terminate(&mut self) -> Result<()> {
        self.process.terminate()
    }
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("WebKit protocol value has no string {field}"))
}

fn protocol_error(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown WebKit protocol error")
        .to_owned()
}

#[derive(Deserialize)]
struct WebKitProfile {
    #[serde(default)]
    samples: Option<WebKitSamples>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebKitSamples {
    #[serde(default)]
    stack_traces: Vec<WebKitStackTrace>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebKitStackTrace {
    timestamp: f64,
    stack_frames: Vec<WebKitFrame>,
}

#[derive(Deserialize)]
struct WebKitFrame {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    line: i64,
    column: i64,
}

fn parse_profile(value: &Value) -> Result<WebKitProfile> {
    serde_json::from_value(value.clone()).context("WebKit emitted an invalid CPU profile")
}

fn profile_traces(value: &Value) -> Vec<WebKitStackTrace> {
    parse_profile(value)
        .ok()
        .and_then(|profile| profile.samples)
        .map(|samples| samples.stack_traces)
        .unwrap_or_default()
}

fn target_traces(value: &Value, target_url: &str) -> Result<Vec<WebKitStackTrace>> {
    Ok(parse_profile(value)?
        .samples
        .map(|samples| samples.stack_traces)
        .unwrap_or_default()
        .into_iter()
        .filter(|trace| {
            trace
                .stack_frames
                .iter()
                .any(|frame| is_benchmark_code_url(&frame.url) || frame.url.starts_with(target_url))
        })
        .collect())
}

fn benchmark_profile_cpu_milliseconds(value: &Value, target_url: &str) -> Result<f64> {
    let count = target_traces(value, target_url)?.len();
    Ok(count as f64)
}

fn parse_live_heap_bytes(snapshot_data: &str) -> Result<u64> {
    let snapshot: Value =
        serde_json::from_str(snapshot_data).context("WebKit emitted invalid heap JSON")?;
    let nodes = snapshot
        .get("nodes")
        .and_then(Value::as_array)
        .context("WebKit emitted an invalid heap snapshot")?;
    if nodes.is_empty() || nodes.len() % 4 != 0 {
        bail!("WebKit emitted an invalid heap snapshot");
    }
    let mut total = 0_u64;
    for node in nodes.chunks_exact(4) {
        let size = node[1]
            .as_u64()
            .context("WebKit heap snapshot contains an invalid node size")?;
        total = total
            .checked_add(size)
            .context("WebKit heap snapshot size overflowed")?;
    }
    if total == 0 {
        bail!("WebKit heap snapshot contains no live heap bytes");
    }
    Ok(total)
}

fn finish_capture_artifacts(
    artifacts: CaptureArtifacts,
    profile: &Value,
    snapshot: &str,
    target_url: Option<&str>,
) -> Result<Vec<ArtifactEvidence>> {
    artifacts.write_cpu_profile(serde_json::to_vec(profile)?)?;
    artifacts.write_heap_snapshot(snapshot)?;
    let flamegraph = webkit_speedscope(profile, target_url)?;
    artifacts.write_flamegraph(&flamegraph)?;
    artifacts.finish()
}

fn webkit_speedscope(profile: &Value, target_url: Option<&str>) -> Result<SpeedscopeDocument> {
    let traces = if let Some(target_url) = target_url {
        target_traces(profile, target_url)?
    } else {
        parse_profile(profile)?
            .samples
            .map(|samples| samples.stack_traces)
            .unwrap_or_default()
    };
    if traces.is_empty() {
        if target_url.is_some() {
            return webkit_speedscope(profile, None);
        }
        bail!("WebKit CPU profile contains no Speedscope samples");
    }
    let mut builder = SpeedscopeBuilder::new("WebKit CPU", "bperf Rust WebKit adapter");
    let mut samples = Vec::with_capacity(traces.len());
    for trace in &traces {
        let mut stack = Vec::with_capacity(trace.stack_frames.len());
        for frame in trace.stack_frames.iter().rev() {
            let normalized = SpeedscopeFrame {
                name: if frame.name.is_empty() {
                    "(anonymous)".to_owned()
                } else {
                    frame.name.clone()
                },
                file: (!frame.url.is_empty()).then(|| frame.url.clone()),
                line: Some(frame.line),
                col: Some(frame.column),
            };
            stack.push(builder.frame(normalized));
        }
        if stack.is_empty() {
            bail!("WebKit CPU profile contains an empty Speedscope stack");
        }
        samples.push(stack);
    }
    let weights = if target_url.is_some() {
        vec![0.001; traces.len()]
    } else {
        positive_weights(
            &traces
                .iter()
                .map(|trace| trace.timestamp)
                .collect::<Vec<_>>(),
            0.001,
        )?
    };
    builder.sampled_profile(
        "WebKit renderer JavaScript",
        "seconds",
        0.0,
        samples,
        weights,
    )?;
    builder.finish()
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::{BTreeMap, VecDeque},
        io::{Read as _, Write as _},
        net::{TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    use crate::lab::{BrowserLab, Engine};

    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        sent: Vec<Value>,
    }

    impl InspectorTransport for FakeTransport {
        fn send(&mut self, message: &Value) -> Result<()> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn receive(&self, _timeout: Duration) -> Result<Value> {
            bail!("fake inspector has no incoming messages")
        }

        fn wait_for_exit(&mut self) -> Result<()> {
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct ScriptedTransport {
        sent: Vec<Value>,
        incoming: RefCell<VecDeque<Value>>,
    }

    impl InspectorTransport for ScriptedTransport {
        fn send(&mut self, message: &Value) -> Result<()> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn receive(&self, _timeout: Duration) -> Result<Value> {
            self.incoming
                .borrow_mut()
                .pop_front()
                .context("scripted inspector has no incoming message")
        }

        fn wait_for_exit(&mut self) -> Result<()> {
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn target_dispatch(nested: Value) -> Value {
        json!({
            "pageProxyId": "proxy",
            "method": "Target.dispatchMessageFromTarget",
            "params": {
                "targetId": "target",
                "message": serde_json::to_string(&nested).unwrap(),
            }
        })
    }

    fn worker_dispatch(nested: Value) -> Value {
        target_dispatch(json!({
            "method": "Worker.dispatchMessageFromWorker",
            "params": {
                "workerId": "worker",
                "message": serde_json::to_string(&nested).unwrap(),
            }
        }))
    }

    fn route(current: &str, provisional: Option<&str>) -> PageRoute {
        let mut targets = HashMap::new();
        targets.insert(
            current.to_owned(),
            TargetRoute {
                main_frame_id: Some("main".to_owned()),
                ..TargetRoute::default()
            },
        );
        if let Some(provisional) = provisional {
            targets.insert(provisional.to_owned(), TargetRoute::default());
        }
        PageRoute {
            browser_context_id: "context".to_owned(),
            config: default_browser_config(),
            current_target: Some(current.to_owned()),
            provisional_target: provisional.map(str::to_owned),
            targets,
            capture_phase: WebKitCapturePhase::Idle,
            next_worker_scope: 1,
            proxy_initialized: true,
        }
    }

    struct FreshStateServer {
        url: String,
        running: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FreshStateServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let running = Arc::new(AtomicBool::new(true));
            let thread_running = Arc::clone(&running);
            let thread = thread::spawn(move || {
                let document = br#"<!doctype html><script>
const previous = localStorage.getItem("bperf-context");
localStorage.setItem("bperf-context", "used");
globalThis.__bperfDescription = { fresh: previous === null };
globalThis.__bperf = {
  run() {
    let total = 0;
    const deadline = performance.now() + 100;
    while (performance.now() < deadline) {
      for (let index = 0; index < 10_000; index += 1) {
        total += Math.sqrt(index % 1_000);
      }
    }
    globalThis.__bperfParityHeap ??= [];
    globalThis.__bperfParityHeap.push(new Array(1_000).fill(total));
    return previous === null;
  }
};
</script>"#;
                while thread_running.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut request = [0_u8; 4096];
                            let _ = stream.read(&mut request);
                            let headers = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                document.len()
                            );
                            let _ = stream.write_all(headers.as_bytes());
                            let _ = stream.write_all(document);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url: format!("http://{address}/"),
                running,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FreshStateServer {
        fn drop(&mut self) {
            self.running.store(false, Ordering::Release);
            let _ =
                TcpStream::connect(self.url.trim_end_matches('/').trim_start_matches("http://"));
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("sidecar")
            .join("test")
            .join("fixtures")
            .join("captures")
            .join("webkit")
            .join(name)
    }

    fn synthetic_private_protocol(excluded: Option<&str>) -> Value {
        let mut domains = BTreeMap::<String, (Vec<Value>, Vec<Value>)>::new();
        for (members, command) in [
            (REQUIRED_PROTOCOL_COMMANDS, true),
            (REQUIRED_PROTOCOL_EVENTS, false),
        ] {
            for qualified in members
                .iter()
                .copied()
                .filter(|qualified| Some(*qualified) != excluded)
            {
                let (domain, name) = qualified.split_once('.').unwrap();
                let parameters = REQUIRED_PROTOCOL_PARAMETERS
                    .iter()
                    .find(|(member, _)| *member == qualified)
                    .map(|(_, parameters)| {
                        parameters
                            .iter()
                            .map(|name| json!({"name": name}))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let member = json!({"name": name, "parameters": parameters});
                let entry = domains.entry(domain.to_owned()).or_default();
                if command {
                    entry.0.push(member);
                } else {
                    entry.1.push(member);
                }
            }
        }
        Value::Array(
            domains
                .into_iter()
                .map(|(domain, (commands, events))| {
                    json!({
                        "domain": domain,
                        "commands": commands,
                        "events": events,
                    })
                })
                .collect(),
        )
    }

    #[test]
    fn private_protocol_preflight_rejects_missing_commands() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        serde_json::to_writer(&mut file, &synthetic_private_protocol(None)).unwrap();
        validate_private_protocol(file.path()).unwrap();

        let mut missing = tempfile::NamedTempFile::new().unwrap();
        serde_json::to_writer(
            &mut missing,
            &synthetic_private_protocol(Some("Page.overrideUserAgent")),
        )
        .unwrap();
        let error = validate_private_protocol(missing.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Page.overrideUserAgent"));
        assert!(error.contains("incompatible"));
    }

    #[test]
    fn golden_webkit_capture_preserves_metrics_and_flamegraph_shape() {
        let profile: Value =
            serde_json::from_slice(&fs::read(fixture("cpu.json")).unwrap()).unwrap();
        let heap = fs::read_to_string(fixture("heap.json")).unwrap();
        assert_eq!(
            benchmark_profile_cpu_milliseconds(&profile, "http://127.0.0.1:4317/").unwrap(),
            2.0
        );
        assert_eq!(parse_live_heap_bytes(&heap).unwrap(), 96);

        let actual = serde_json::to_value(
            webkit_speedscope(&profile, Some("http://127.0.0.1:4317/")).unwrap(),
        )
        .unwrap();
        let mut expected: Value =
            serde_json::from_slice(&fs::read(fixture("flamegraph.json")).unwrap()).unwrap();
        expected["exporter"] = Value::String("bperf Rust WebKit adapter".to_owned());
        assert_eq!(actual, expected);
    }

    #[test]
    fn malformed_webkit_captures_fail_explicitly() {
        assert!(parse_live_heap_bytes("{}").is_err());
        assert!(parse_live_heap_bytes(r#"{"nodes":[1,2,3]}"#).is_err());
        assert!(parse_live_heap_bytes(r#"{"nodes":[1,-1,0,0]}"#).is_err());
        assert_eq!(
            benchmark_profile_cpu_milliseconds(
                &json!({"samples":{"stackTraces":[]}}),
                "http://127.0.0.1/"
            )
            .unwrap(),
            0.0
        );
    }

    #[test]
    fn profiler_accepts_unavailable_source_locations() {
        let profile = json!({
            "samples": {
                "stackTraces": [{
                    "timestamp": 1.0,
                    "stackFrames": [{
                        "name": "(program)",
                        "url": "http://127.0.0.1:4317/benchmark.js",
                        "line": -1,
                        "column": -1
                    }]
                }]
            }
        });

        let speedscope = serde_json::to_value(webkit_speedscope(&profile, None).unwrap()).unwrap();
        assert_eq!(speedscope["shared"]["frames"][0]["line"], -1);
        assert_eq!(speedscope["shared"]["frames"][0]["col"], -1);
    }

    #[test]
    fn nested_target_responses_and_events_route_to_their_page() {
        let mut connection = InspectorConnection::new(
            FakeTransport::default(),
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        connection
            .pages
            .insert("proxy".to_owned(), route("target", None));
        connection
            .dispatch_page_message(
                "proxy",
                json!({
                    "method": "Target.dispatchMessageFromTarget",
                    "params": {
                        "targetId": "target",
                        "message": serde_json::to_string(&json!({
                            "id": 41,
                            "result": {"value": 42}
                        })).unwrap()
                    }
                }),
            )
            .unwrap();
        assert_eq!(
            connection.responses.remove(&41).unwrap().unwrap(),
            json!({"value": 42})
        );

        connection
            .dispatch_target_event(
                "proxy",
                "target",
                json!({
                    "method": "Page.loadEventFired",
                    "params": {"frameId": "child"}
                }),
            )
            .unwrap();
        assert!(!connection.pages["proxy"].targets["target"].load_fired);

        connection
            .dispatch_page_message(
                "proxy",
                json!({
                    "method": "Target.dispatchMessageFromTarget",
                    "params": {
                        "targetId": "target",
                        "message": serde_json::to_string(&json!({
                            "method": "Page.loadEventFired",
                            "params": {"frameId": "main"}
                        })).unwrap()
                    }
                }),
            )
            .unwrap();
        assert!(
            connection.pages["proxy"].targets["target"].load_fired,
            "nested page events must update the matching target"
        );
    }

    #[test]
    fn nested_worker_responses_profiles_and_scopes_route_to_their_worker() {
        let mut connection = InspectorConnection::new(
            FakeTransport::default(),
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        let mut page = route("target", None);
        page.targets.get_mut("target").unwrap().workers.insert(
            "worker".to_owned(),
            WorkerRoute {
                capture_scope: "worker-1".to_owned(),
                url: "http://127.0.0.1:4317/worker.js".to_owned(),
                profile: None,
                profiler_started: true,
            },
        );
        connection.pages.insert("proxy".to_owned(), page);

        connection
            .dispatch_page_message(
                "proxy",
                worker_dispatch(json!({
                    "id": 41,
                    "result": {"value": 42}
                })),
            )
            .unwrap();
        assert_eq!(
            connection.responses.remove(&41).unwrap().unwrap(),
            json!({"value": 42})
        );

        let profile = json!({"samples": {"stackTraces": []}});
        connection
            .dispatch_page_message(
                "proxy",
                worker_dispatch(json!({
                    "method": "ScriptProfiler.trackingComplete",
                    "params": profile
                })),
            )
            .unwrap();
        assert_eq!(
            connection.pages["proxy"].targets["target"].workers["worker"].profile,
            Some(profile)
        );

        let realms = connection.capture_realms("proxy").unwrap();
        assert_eq!(
            realms
                .iter()
                .map(|realm| realm.capture_scope.as_str())
                .collect::<Vec<_>>(),
            ["page", "worker-1"]
        );
    }

    #[test]
    fn separate_webkit_child_targets_fail_instead_of_losing_evidence() {
        let mut connection = InspectorConnection::new(
            FakeTransport::default(),
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        connection
            .pages
            .insert("proxy".to_owned(), route("target", None));
        let error = connection
            .dispatch_page_message(
                "proxy",
                json!({
                    "method": "Target.targetCreated",
                    "params": {
                        "targetInfo": {
                            "targetId": "frame",
                            "type": "frame"
                        }
                    }
                }),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("separate frame target"));
        assert!(error.contains("complete capture contract"));
    }

    #[test]
    fn provisional_commit_atomically_changes_the_current_target() {
        let mut connection = InspectorConnection::new(
            FakeTransport::default(),
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        connection
            .pages
            .insert("proxy".to_owned(), route("old", Some("new")));
        connection
            .dispatch_page_message(
                "proxy",
                json!({
                    "method": "Target.didCommitProvisionalTarget",
                    "params": {
                        "oldTargetId": "old",
                        "newTargetId": "new"
                    }
                }),
            )
            .unwrap();

        let page = &connection.pages["proxy"];
        assert_eq!(page.current_target.as_deref(), Some("new"));
        assert_eq!(page.provisional_target, None);
    }

    #[test]
    fn async_evaluation_uses_webkits_await_promise_command() {
        let mut transport = ScriptedTransport::default();
        transport.incoming.get_mut().extend([
            json!({"id": 2, "result": {}, "pageProxyId": "proxy"}),
            target_dispatch(json!({
                "id": 1,
                "result": {
                    "result": {
                        "type": "object",
                        "className": "Promise",
                        "objectId": "promise-1"
                    }
                }
            })),
            json!({"id": 4, "result": {}, "pageProxyId": "proxy"}),
            target_dispatch(json!({
                "id": 3,
                "result": {
                    "result": {"type": "number", "value": 42}
                }
            })),
        ]);
        let mut connection = InspectorConnection::new(
            transport,
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        connection
            .pages
            .insert("proxy".to_owned(), route("target", None));

        assert_eq!(
            connection.evaluate("proxy", "Promise.resolve(42)").unwrap(),
            json!(42)
        );

        let methods = connection
            .process
            .sent
            .iter()
            .map(|outer| {
                serde_json::from_str::<Value>(outer["params"]["message"].as_str().unwrap()).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(methods[0]["method"], "Runtime.evaluate");
        assert!(methods[0]["params"].get("awaitPromise").is_none());
        assert!(
            methods[0]["params"]["expression"]
                .as_str()
                .unwrap()
                .starts_with("(async () => (")
        );
        assert_eq!(methods[1]["method"], "Runtime.awaitPromise");
        assert_eq!(methods[1]["params"]["promiseObjectId"], "promise-1");
        assert_eq!(methods[2]["method"], "Runtime.releaseObject");
    }

    #[test]
    fn external_requests_are_failed_inside_the_target_session() {
        let mut connection = InspectorConnection::new(
            FakeTransport::default(),
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        connection
            .pages
            .insert("proxy".to_owned(), route("target", None));
        connection
            .dispatch_target_event(
                "proxy",
                "target",
                json!({
                    "method": "Network.requestWillBeSent",
                    "params": {
                        "requestId": "request",
                        "request": {"url": "https://example.com/data"}
                    }
                }),
            )
            .unwrap();
        connection
            .dispatch_target_event(
                "proxy",
                "target",
                json!({
                    "method": "Network.requestIntercepted",
                    "params": {
                        "requestId": "request"
                    }
                }),
            )
            .unwrap();

        let outer = connection.process.sent.last().unwrap();
        assert_eq!(outer["method"], "Target.sendMessageToTarget");
        let nested: Value =
            serde_json::from_str(outer["params"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(nested["method"], "Network.interceptRequestWithError");
        assert_eq!(nested["params"]["errorType"], "Cancellation");

        connection
            .dispatch_target_event(
                "proxy",
                "target",
                json!({
                    "method": "Network.requestIntercepted",
                    "params": {"requestId": "loopback"}
                }),
            )
            .unwrap();
        assert_eq!(connection.process.sent.len(), 1);
        connection
            .dispatch_target_event(
                "proxy",
                "target",
                json!({
                    "method": "Network.requestWillBeSent",
                    "params": {
                        "requestId": "loopback",
                        "request": {"url": "http://127.0.0.1:4317/data"}
                    }
                }),
            )
            .unwrap();
        let outer = connection.process.sent.last().unwrap();
        let nested: Value =
            serde_json::from_str(outer["params"]["message"].as_str().unwrap()).unwrap();
        assert_eq!(nested["method"], "Network.interceptWithRequest");
    }

    #[test]
    fn failed_background_protocol_commands_poison_the_lane() {
        let mut connection = InspectorConnection::new(
            FakeTransport::default(),
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        connection
            .send_target_without_waiting(
                "proxy",
                "target",
                "Network.interceptRequestWithError",
                json!({"requestId": "request", "errorType": "Cancellation"}),
            )
            .unwrap();
        let outer = connection.process.sent.last().unwrap();
        let nested: Value =
            serde_json::from_str(outer["params"]["message"].as_str().unwrap()).unwrap();
        let nested_id = nested["id"].as_u64().unwrap();

        connection.store_response(
            nested_id,
            &json!({
                "id": nested_id,
                "error": {"message": "interception failed"}
            }),
        );

        let error = connection.check_fatal_error().unwrap_err().to_string();
        assert!(error.contains("Network.interceptRequestWithError"));
        assert!(error.contains("interception failed"));
    }

    #[test]
    fn expected_context_deletion_does_not_poison_the_retained_lane() {
        let mut connection = InspectorConnection::new(
            FakeTransport::default(),
            default_user_agent("26.5"),
            PathBuf::from("downloads"),
        );
        connection
            .pages
            .insert("expected".to_owned(), route("target", None));
        connection
            .pages
            .insert("popup".to_owned(), route("popup-target", None));
        connection.closing_contexts.insert("context".to_owned());
        connection
            .dispatch_browser_event(json!({
                "method": "Playwright.pageProxyDestroyed",
                "params": {"pageProxyId": "expected"}
            }))
            .unwrap();
        connection
            .dispatch_browser_event(json!({
                "method": "Playwright.pageProxyDestroyed",
                "params": {"pageProxyId": "popup"}
            }))
            .unwrap();
        assert_eq!(connection.fatal_error, None);

        let mut unexpected = route("target", None);
        unexpected.browser_context_id = "other-context".to_owned();
        connection.pages.insert("unexpected".to_owned(), unexpected);
        connection
            .dispatch_browser_event(json!({
                "method": "Playwright.pageProxyDestroyed",
                "params": {"pageProxyId": "unexpected"}
            }))
            .unwrap();
        assert!(
            connection
                .fatal_error
                .as_deref()
                .is_some_and(|error| error.contains("unexpected"))
        );
    }

    #[test]
    #[ignore = "launches the pinned Playwright WebKit browser"]
    fn browser_lab_uses_fresh_contexts_and_recovers_after_failure() {
        let server = FreshStateServer::start();
        let mut browser_lab = BrowserLab::start(RuntimeInstallation::discover().unwrap()).unwrap();

        let first = browser_lab
            .inspect_benchmark(Engine::Webkit, &server.url, None)
            .unwrap();
        let second = browser_lab
            .inspect_benchmark(Engine::Webkit, &server.url, None)
            .unwrap();
        assert_eq!(first.description["fresh"], true);
        assert_eq!(second.description["fresh"], true);

        browser_lab
            .inspect_benchmark(Engine::Webkit, "https://example.com/", None)
            .unwrap_err();

        let reopened = browser_lab
            .inspect_benchmark(Engine::Webkit, &server.url, None)
            .unwrap();
        assert_eq!(reopened.description["fresh"], true);
        browser_lab.finish().unwrap();
    }
}
