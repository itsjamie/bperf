//! Direct adapter for Playwright's pinned Chromium headless shell.
//!
//! Process ownership, CDP routing, workload execution, native capture parsing,
//! and artifact normalization stay behind the engine-neutral browser laboratory
//! interface.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bperf_runtime::installation::{BrowserName, RuntimeInstallation};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    artifacts::{
        CaptureArtifacts, SpeedscopeBuilder, SpeedscopeDocument, SpeedscopeFrame, sha256_file,
    },
    browser_process::BrowserProcess,
    browser_workload::{
        BENCHMARK_DESCRIPTION_EXPRESSION, BENCHMARK_READY_EXPRESSION, DOCTOR_PROBE_EXPRESSION,
        RUNTIME_ANCHOR_EXPRESSION, SETTLE_EXPRESSION, VERSION as BROWSER_WORKLOAD_VERSION,
        WORKLOAD_READY_EXPRESSION, WorkloadScript, bootstrap_source, decode_batch_size,
        decode_runtime_anchor, decode_workload, default_browser_config, installed_expression,
        is_allowed_adapter_url, is_allowed_trial_url,
    },
    lab::{
        AdapterEvidence, AdapterTrialRequest, ArtifactEvidence, BenchmarkInspection,
        BrowserEvidence, BrowserTrialConfig, Engine, EngineAdapter, EngineLane, ProbeCapture,
        TrialCapture,
    },
};

pub(crate) const ADAPTER_PROTOCOL_VERSION: u32 = 1;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(300);
const PAGE_READY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct ChromiumAdapter {
    executable: PathBuf,
    revision: String,
    browser_version: String,
    playwright_version: String,
    executable_sha256: String,
    launch_arguments: Vec<String>,
}

impl EngineAdapter for ChromiumAdapter {
    type Lane = ChromiumLane;

    fn discover(installation: &RuntimeInstallation) -> Result<Self> {
        let chromium = installation.browser(BrowserName::ChromiumHeadlessShell)?;
        let executable = chromium_executable(chromium.directory())?;
        if !executable.is_file() {
            bail!(
                "Playwright Chromium revision {} is not installed at {}; run `npx playwright install chromium` for the pinned sidecar",
                chromium.revision(),
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
                    "Playwright Chromium executable is not executable: {}",
                    executable.display()
                );
            }
        }

        Ok(Self {
            executable_sha256: sha256_file(&executable)?,
            executable,
            revision: chromium.revision().to_owned(),
            browser_version: chromium.browser_version().to_owned(),
            playwright_version: installation.playwright_version().to_owned(),
            launch_arguments: chromium_launch_arguments(),
        })
    }

    fn launch(&self) -> Result<Self::Lane> {
        ChromiumLane::launch(self)
    }
}

impl ChromiumAdapter {
    fn adapter_evidence(&self) -> AdapterEvidence {
        AdapterEvidence::Chromium {
            playwright: self.playwright_version.clone(),
            chromium_revision: self.revision.clone(),
            executable_sha256: self.executable_sha256.clone(),
            protocol_version: ADAPTER_PROTOCOL_VERSION,
            browser_workload_version: BROWSER_WORKLOAD_VERSION,
        }
    }
}

fn chromium_executable(browser_directory: &Path) -> Result<PathBuf> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Ok(browser_directory
            .join("chrome-headless-shell-win64")
            .join("chrome-headless-shell.exe"))
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok(browser_directory
            .join("chrome-headless-shell-linux64")
            .join("chrome-headless-shell"))
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Ok(browser_directory
            .join("chrome-linux")
            .join("headless_shell"))
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Ok(browser_directory
            .join("chrome-headless-shell-mac-x64")
            .join("chrome-headless-shell"))
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok(browser_directory
            .join("chrome-headless-shell-mac-arm64")
            .join("chrome-headless-shell"))
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
    )))]
    {
        let _ = browser_directory;
        bail!("Playwright Chromium is unsupported on this platform")
    }
}

fn chromium_launch_arguments() -> Vec<String> {
    [
        "--disable-field-trial-config",
        "--disable-background-networking",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-back-forward-cache",
        "--disable-breakpad",
        "--disable-client-side-phishing-detection",
        "--disable-component-extensions-with-background-pages",
        "--disable-component-update",
        "--no-default-browser-check",
        "--disable-default-apps",
        "--disable-dev-shm-usage",
        "--disable-edgeupdater",
        "--disable-extensions",
        "--disable-features=AvoidUnnecessaryBeforeUnloadCheckSync,BoundaryEventDispatchTracksNodeRemoval,DestroyProfileOnBrowserClose,DialMediaRouteProvider,GlobalMediaControls,HttpsUpgrades,LensOverlay,MediaRouter,PaintHolding,ThirdPartyStoragePartitioning,Translate,AutoDeElevate,RenderDocument,OptimizationHints,msForceBrowserSignIn,msEdgeUpdateLaunchServicesPreferredVersion",
        "--enable-features=CDPScreenshotNewSurface",
        "--allow-pre-commit-input",
        "--disable-hang-monitor",
        "--disable-ipc-flooding-protection",
        "--disable-popup-blocking",
        "--disable-prompt-on-repost",
        "--disable-renderer-backgrounding",
        "--force-color-profile=srgb",
        "--metrics-recording-only",
        "--no-first-run",
        "--password-store=basic",
        "--use-mock-keychain",
        "--no-service-autorun",
        "--export-tagged-pdf",
        "--disable-search-engine-choice-screen",
        "--unsafely-disable-devtools-self-xss-warnings",
        "--edge-skip-compat-layer-relaunch",
        "--disable-infobars",
        "--disable-search-engine-choice-screen",
        "--disable-sync",
        "--enable-unsafe-swiftshader",
        "--headless",
        "--hide-scrollbars",
        "--mute-audio",
        "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4",
        "--no-sandbox",
        "--user-data-dir=profile",
        "--remote-debugging-pipe",
        "--no-startup-window",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub(crate) struct ChromiumLane {
    connection: CdpConnection<BrowserProcess>,
    browser: BrowserEvidence,
    adapter: AdapterEvidence,
    closed: bool,
}

impl ChromiumLane {
    fn launch(installation: &ChromiumAdapter) -> Result<Self> {
        let process = BrowserProcess::spawn(
            "bperf-chromium-",
            "rust-chromium",
            &installation.executable,
            &installation.launch_arguments,
        )?;
        let download_directory = process.working_directory().join("downloads");
        fs::create_dir(&download_directory)
            .context("failed to create the isolated Chromium download directory")?;
        let root_pid = process.pid();
        let mut connection = CdpConnection::new(process, download_directory);
        let version = connection
            .send_root("Browser.getVersion", json!({}), REQUEST_TIMEOUT)
            .context("Chromium rejected Browser.getVersion")?;
        let product = required_string(&version, "product")?;
        let actual_version = product
            .split_once('/')
            .map_or(product.as_str(), |(_, version)| version)
            .to_owned();
        if actual_version != installation.browser_version {
            bail!(
                "pinned Chromium revision {} reports version {}, expected {}",
                installation.revision,
                actual_version,
                installation.browser_version
            );
        }
        let browser = BrowserEvidence {
            root_pid,
            executable_path: installation.executable.to_string_lossy().into_owned(),
            version: actual_version,
            launch_args: installation.launch_arguments.clone(),
        };
        Ok(Self {
            connection,
            browser,
            adapter: installation.adapter_evidence(),
            closed: false,
        })
    }

    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture> {
        let page = self.connection.open_page(&default_browser_config())?;
        let result = (|| {
            let artifacts = CaptureArtifacts::prepare(Engine::Chromium, artifact_directory)?;
            let anchor = decode_runtime_anchor(
                self.connection
                    .evaluate(&page.session_id, RUNTIME_ANCHOR_EXPRESSION)?,
            )
            .context("Chromium runtime anchor capture failed")?;
            self.connection.send_session(
                &page.session_id,
                "Profiler.enable",
                json!({}),
                REQUEST_TIMEOUT,
            )?;
            self.connection.send_session(
                &page.session_id,
                "Profiler.start",
                json!({}),
                REQUEST_TIMEOUT,
            )?;
            self.connection
                .evaluate(&page.session_id, DOCTOR_PROBE_EXPRESSION)?;
            let stopped = self.connection.send_session(
                &page.session_id,
                "Profiler.stop",
                json!({}),
                CAPTURE_TIMEOUT,
            )?;
            let profile = stopped
                .get("profile")
                .cloned()
                .context("Chromium CPU capture returned no profile")?;
            let parsed = parse_cpu_profile(&profile)?;
            if parsed.samples.len() < 50 {
                bail!("Chromium CPU profile did not contain enough samples");
            }
            self.connection
                .evaluate(&page.session_id, SETTLE_EXPRESSION)?;
            let heap_path = artifacts.heap_snapshot_path();
            self.connection
                .capture_heap_snapshot(&page.session_id, &heap_path)?;
            parse_live_heap_bytes(&heap_path)?;
            let artifacts = finish_capture_artifacts(artifacts, &profile, &parsed, None)?;
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
            let artifacts = CaptureArtifacts::prepare(Engine::Chromium, request.artifact_root)?;
            self.connection
                .navigate(&page.session_id, request.target_url)?;
            self.connection.wait_for_expression(
                &page.session_id,
                WORKLOAD_READY_EXPRESSION,
                PAGE_READY_TIMEOUT,
            )?;
            let script = WorkloadScript::new(request.operations)?;
            self.connection
                .evaluate(&page.session_id, &script.prepare())?;
            let selected = self.connection.evaluate(
                &page.session_id,
                &script.select_batch_size(request.batches)?,
            )?;
            let batch_size =
                decode_batch_size(selected).context("Chromium batch calibration failed")?;

            self.connection.send_session(
                &page.session_id,
                "Profiler.enable",
                json!({}),
                REQUEST_TIMEOUT,
            )?;
            self.connection.send_session(
                &page.session_id,
                "Profiler.start",
                json!({}),
                REQUEST_TIMEOUT,
            )?;
            let workload = decode_workload(
                self.connection
                    .evaluate(&page.session_id, &script.execute(batch_size))?,
            )
            .context("Chromium workload execution failed")?;
            let stopped = self.connection.send_session(
                &page.session_id,
                "Profiler.stop",
                json!({}),
                CAPTURE_TIMEOUT,
            )?;
            let profile = stopped
                .get("profile")
                .cloned()
                .context("Chromium CPU capture returned no profile")?;
            let parsed = parse_cpu_profile(&profile)?;
            if parsed.samples.is_empty() {
                bail!("Chromium CPU profile did not contain samples");
            }
            let cpu_active_ms =
                cpu_active_milliseconds(&parsed, request.target_url)? / f64::from(batch_size);

            self.connection
                .evaluate(&page.session_id, SETTLE_EXPRESSION)?;
            let heap_path = artifacts.heap_snapshot_path();
            self.connection
                .capture_heap_snapshot(&page.session_id, &heap_path)?;
            let heap_bytes = parse_live_heap_bytes(&heap_path)?;
            let artifacts =
                finish_capture_artifacts(artifacts, &profile, &parsed, Some(request.target_url))?;
            Ok(TrialCapture {
                workload,
                cpu_active_ms,
                js_heap_live_bytes: heap_bytes,
                artifacts,
            })
        })();
        combine_page_close(result, self.connection.close_page(page))
    }

    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection> {
        let page = self.connection.open_page(&default_browser_config())?;
        let result = (|| {
            self.connection.navigate(&page.session_id, target_url)?;
            self.connection.wait_for_expression(
                &page.session_id,
                BENCHMARK_READY_EXPRESSION,
                PAGE_READY_TIMEOUT,
            )?;
            let description = self
                .connection
                .evaluate(&page.session_id, BENCHMARK_DESCRIPTION_EXPRESSION)?;
            if description.is_null() {
                bail!("Chromium benchmark page returned no description");
            }
            let result = if let Some(case_id) = case_id {
                let script = WorkloadScript::new(&[json!({"case_id": case_id})])?;
                self.connection
                    .evaluate(&page.session_id, &script.inspect_result())?
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

impl EngineLane for ChromiumLane {
    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture> {
        ChromiumLane::probe(self, artifact_directory)
    }

    fn measure_trial(&mut self, request: AdapterTrialRequest<'_>) -> Result<TrialCapture> {
        ChromiumLane::measure_trial(self, request)
    }

    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection> {
        ChromiumLane::inspect_benchmark(self, target_url, case_id)
    }

    fn close(&mut self) -> Result<()> {
        ChromiumLane::close(self)
    }

    fn terminate(&mut self) -> Result<()> {
        ChromiumLane::terminate(self)
    }
}

impl Drop for ChromiumLane {
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
        (Ok(_), Err(close_error)) => {
            Err(close_error.context("failed to close Chromium trial state"))
        }
        (Err(error), Err(close_error)) => Err(error.context(format!(
            "Chromium capture also failed to close its isolated context: {close_error:#}"
        ))),
    }
}

struct OpenPage {
    browser_context_id: String,
    target_id: String,
    session_id: String,
}

#[derive(Default)]
struct SessionState {
    load_fired: bool,
}

trait CdpTransport {
    fn send(&mut self, message: &Value) -> Result<()>;
    fn receive(&self, timeout: Duration) -> Result<Value>;
    fn wait_for_exit(&mut self) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
}

impl CdpTransport for BrowserProcess {
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

struct HeapCapture {
    file: File,
    chunks: usize,
}

struct CdpConnection<Transport: CdpTransport> {
    process: Transport,
    download_directory: PathBuf,
    next_id: u64,
    responses: HashMap<u64, std::result::Result<Value, String>>,
    ignored_responses: HashSet<u64>,
    sessions: HashMap<String, SessionState>,
    primary_sessions: HashSet<String>,
    heap_capture: Option<HeapCapture>,
    fatal_error: Option<String>,
}

impl<Transport: CdpTransport> CdpConnection<Transport> {
    fn new(process: Transport, download_directory: PathBuf) -> Self {
        Self {
            process,
            download_directory,
            next_id: 1,
            responses: HashMap::new(),
            ignored_responses: HashSet::new(),
            sessions: HashMap::new(),
            primary_sessions: HashSet::new(),
            heap_capture: None,
            fatal_error: None,
        }
    }

    fn send_root(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.send_command(None, method, params, timeout)
    }

    fn send_session(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.send_command(Some(session_id), method, params, timeout)
    }

    fn send_command(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.check_fatal_error()?;
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("CDP request id overflowed")?;
        let mut message = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            message["sessionId"] = Value::String(session_id.to_owned());
        }
        self.process
            .send(&message)
            .with_context(|| format!("failed to send Chromium CDP command {method}"))?;
        self.wait_for_response(id, method, Instant::now() + timeout)
    }

    fn send_ignored(&mut self, session_id: &str, method: &str, params: Value) -> Result<()> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("CDP request id overflowed")?;
        self.ignored_responses.insert(id);
        self.process.send(&json!({
            "id": id,
            "sessionId": session_id,
            "method": method,
            "params": params,
        }))
    }

    fn wait_for_response(&mut self, id: u64, method: &str, deadline: Instant) -> Result<Value> {
        loop {
            if let Some(response) = self.responses.remove(&id) {
                return response.map_err(|message| anyhow::anyhow!("{method} failed: {message}"));
            }
            self.check_fatal_error()?;
            self.pump(deadline)?;
        }
    }

    fn pump(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("Chromium CDP request timed out");
        }
        let message = self.process.receive(remaining)?;
        if let Some(action) = self.dispatch(message)? {
            self.handle_action(action)?;
        }
        Ok(())
    }

    fn dispatch(&mut self, message: Value) -> Result<Option<CdpAction>> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let result = if let Some(error) = message.get("error") {
                Err(protocol_error(error))
            } else {
                Ok(message.get("result").cloned().unwrap_or_else(|| json!({})))
            };
            if self.ignored_responses.remove(&id) {
                if let Err(error) = result {
                    self.fatal_error = Some(format!("background CDP command failed: {error}"));
                }
            } else {
                self.responses.insert(id, result);
            }
            return Ok(None);
        }

        let method = message
            .get("method")
            .and_then(Value::as_str)
            .context("Chromium CDP event has no method")?;
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let session_id = message
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match method {
            "Page.loadEventFired" => {
                if let Some(session_id) = session_id {
                    self.sessions.entry(session_id).or_default().load_fired = true;
                }
                Ok(None)
            }
            "Fetch.requestPaused" => {
                let session_id =
                    session_id.context("Chromium Fetch event has no target session")?;
                let request_id = required_string(&params, "requestId")?;
                let url = params
                    .get("request")
                    .and_then(|request| request.get("url"))
                    .and_then(Value::as_str)
                    .context("Chromium Fetch event has no request URL")?
                    .to_owned();
                Ok(Some(CdpAction::Intercept {
                    session_id,
                    request_id,
                    allowed: is_allowed_trial_url(&url),
                }))
            }
            "HeapProfiler.addHeapSnapshotChunk" => {
                let chunk = params
                    .get("chunk")
                    .and_then(Value::as_str)
                    .context("Chromium heap event has no chunk")?;
                let capture = self
                    .heap_capture
                    .as_mut()
                    .context("Chromium emitted a heap chunk outside capture")?;
                capture
                    .file
                    .write_all(chunk.as_bytes())
                    .context("failed to write Chromium heap snapshot")?;
                capture.chunks += 1;
                Ok(None)
            }
            "Target.attachedToTarget" => {
                let child_session = required_string(&params, "sessionId")?;
                let target_type = params
                    .get("targetInfo")
                    .and_then(|info| info.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let waiting = params
                    .get("waitingForDebugger")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if waiting {
                    self.sessions.entry(child_session.clone()).or_default();
                    Ok(Some(CdpAction::InitializeTarget {
                        session_id: child_session,
                        target_type,
                    }))
                } else {
                    Ok(None)
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(detached) = params.get("sessionId").and_then(Value::as_str) {
                    self.sessions.remove(detached);
                    if self.primary_sessions.contains(detached) {
                        self.fatal_error =
                            Some("Chromium detached the active page target".to_owned());
                    }
                }
                Ok(None)
            }
            "Inspector.detached" => {
                if let Some(session_id) = session_id
                    && self.primary_sessions.contains(&session_id)
                {
                    self.fatal_error =
                        Some("Chromium inspector detached from the active page".to_owned());
                }
                Ok(None)
            }
            "Inspector.targetCrashed" | "Target.targetCrashed" => {
                self.fatal_error = Some("Chromium target crashed during capture".to_owned());
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn handle_action(&mut self, action: CdpAction) -> Result<()> {
        match action {
            CdpAction::Intercept {
                session_id,
                request_id,
                allowed,
            } => {
                let (method, params) = if allowed {
                    ("Fetch.continueRequest", json!({"requestId": request_id}))
                } else {
                    (
                        "Fetch.failRequest",
                        json!({
                            "requestId": request_id,
                            "errorReason": "BlockedByClient",
                        }),
                    )
                };
                self.send_ignored(&session_id, method, params)
            }
            CdpAction::InitializeTarget {
                session_id,
                target_type,
            } => self.initialize_autoattached_target(&session_id, &target_type),
        }
    }

    fn initialize_autoattached_target(
        &mut self,
        session_id: &str,
        target_type: &str,
    ) -> Result<()> {
        self.send_session(session_id, "Runtime.enable", json!({}), REQUEST_TIMEOUT)?;
        self.enable_fetch(session_id)?;
        if target_type == "iframe" {
            self.send_session(
                session_id,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": bootstrap_source()}),
                REQUEST_TIMEOUT,
            )?;
        }
        self.install_bootstrap(session_id)?;
        self.send_session(
            session_id,
            "Runtime.runIfWaitingForDebugger",
            json!({}),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    fn open_page(&mut self, config: &BrowserTrialConfig) -> Result<OpenPage> {
        let context = self.send_root(
            "Target.createBrowserContext",
            json!({"disposeOnDetach": true}),
            REQUEST_TIMEOUT,
        )?;
        let browser_context_id = required_string(&context, "browserContextId")?;
        self.send_root(
            "Browser.setDownloadBehavior",
            json!({
                "behavior": "deny",
                "browserContextId": browser_context_id,
                "downloadPath": self.download_directory.to_string_lossy(),
                "eventsEnabled": false,
            }),
            REQUEST_TIMEOUT,
        )?;
        let target = self.send_root(
            "Target.createTarget",
            json!({
                "url": "about:blank",
                "browserContextId": browser_context_id,
            }),
            REQUEST_TIMEOUT,
        )?;
        let target_id = required_string(&target, "targetId")?;
        let attached = self.send_root(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            REQUEST_TIMEOUT,
        )?;
        let session_id = required_string(&attached, "sessionId")?;
        self.sessions.entry(session_id.clone()).or_default();
        self.primary_sessions.insert(session_id.clone());
        if let Err(error) = self.configure_page(&session_id, config) {
            self.primary_sessions.remove(&session_id);
            let _ = self.send_root(
                "Target.disposeBrowserContext",
                json!({"browserContextId": browser_context_id}),
                REQUEST_TIMEOUT,
            );
            return Err(error);
        }
        Ok(OpenPage {
            browser_context_id,
            target_id,
            session_id,
        })
    }

    fn configure_page(&mut self, session_id: &str, config: &BrowserTrialConfig) -> Result<()> {
        for (method, params) in [
            ("Page.enable", json!({})),
            ("Runtime.enable", json!({})),
            ("Network.enable", json!({})),
            ("Network.setCacheDisabled", json!({"cacheDisabled": true})),
            (
                "Network.setExtraHTTPHeaders",
                json!({"headers": {"Accept-Language": config.locale}}),
            ),
            (
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": config.viewport.width,
                    "height": config.viewport.height,
                    "deviceScaleFactor": 1,
                    "mobile": false,
                    "screenWidth": config.viewport.width,
                    "screenHeight": config.viewport.height,
                }),
            ),
            (
                "Emulation.setLocaleOverride",
                json!({"locale": config.locale}),
            ),
            (
                "Emulation.setTimezoneOverride",
                json!({"timezoneId": config.timezone_id}),
            ),
            (
                "Emulation.setEmulatedMedia",
                json!({
                    "media": "",
                    "features": [{
                        "name": "prefers-color-scheme",
                        "value": config.color_scheme,
                    }],
                }),
            ),
            (
                "Emulation.setTouchEmulationEnabled",
                json!({"enabled": false}),
            ),
            (
                "Emulation.setFocusEmulationEnabled",
                json!({"enabled": true}),
            ),
        ] {
            self.send_session(session_id, method, params, REQUEST_TIMEOUT)?;
        }
        self.enable_fetch(session_id)?;
        let bootstrap = bootstrap_source();
        self.send_session(
            session_id,
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": bootstrap}),
            REQUEST_TIMEOUT,
        )?;
        self.install_bootstrap(session_id)?;
        self.send_session(
            session_id,
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                "filter": [
                    {"type": "page", "exclude": true},
                    {"type": "tab", "exclude": true},
                    {"type": "browser", "exclude": true},
                    {"type": "worker"},
                    {"type": "shared_worker"},
                    {"type": "service_worker"},
                    {"type": "iframe"},
                    {"exclude": true},
                ],
            }),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    fn enable_fetch(&mut self, session_id: &str) -> Result<()> {
        self.send_session(
            session_id,
            "Fetch.enable",
            json!({
                "patterns": [{"urlPattern": "*", "requestStage": "Request"}],
                "handleAuthRequests": false,
            }),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    fn navigate(&mut self, session_id: &str, target_url: &str) -> Result<()> {
        if !is_allowed_adapter_url(target_url) {
            bail!("variant adapter returned a non-loopback URL: {target_url}");
        }
        self.sessions
            .get_mut(session_id)
            .context("cannot navigate an unknown Chromium page")?
            .load_fired = false;
        let navigated = self.send_session(
            session_id,
            "Page.navigate",
            json!({"url": target_url}),
            REQUEST_TIMEOUT,
        )?;
        if let Some(error) = navigated.get("errorText").and_then(Value::as_str)
            && !error.is_empty()
        {
            bail!("Chromium navigation failed: {error}");
        }
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        while !self
            .sessions
            .get(session_id)
            .is_some_and(|state| state.load_fired)
        {
            self.check_fatal_error()?;
            self.pump(deadline)?;
        }
        self.wait_for_expression(session_id, &installed_expression(), PAGE_READY_TIMEOUT)
    }

    fn evaluate(&mut self, session_id: &str, expression: &str) -> Result<Value> {
        let evaluated = self.send_session(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": format!("(async () => ({expression}))()"),
                "awaitPromise": true,
                "returnByValue": true,
                "userGesture": true,
            }),
            CAPTURE_TIMEOUT,
        )?;
        if let Some(exception) = evaluated.get("exceptionDetails") {
            let description = exception
                .get("exception")
                .and_then(|exception| exception.get("description"))
                .and_then(Value::as_str)
                .or_else(|| exception.get("text").and_then(Value::as_str))
                .unwrap_or("JavaScript exception");
            bail!("Chromium page evaluation failed: {description}");
        }
        let result = evaluated
            .get("result")
            .context("Chromium page evaluation returned no result")?;
        Ok(result.get("value").cloned().unwrap_or(Value::Null))
    }

    fn install_bootstrap(&mut self, session_id: &str) -> Result<()> {
        let evaluated = self.send_session(
            session_id,
            "Runtime.evaluate",
            json!({
                "expression": bootstrap_source(),
                "awaitPromise": true,
                "returnByValue": true,
            }),
            REQUEST_TIMEOUT,
        )?;
        if let Some(exception) = evaluated.get("exceptionDetails") {
            let description = exception
                .get("exception")
                .and_then(|exception| exception.get("description"))
                .and_then(Value::as_str)
                .or_else(|| exception.get("text").and_then(Value::as_str))
                .unwrap_or("JavaScript exception");
            bail!("Chromium rejected the browser workload bootstrap: {description}");
        }
        Ok(())
    }

    fn wait_for_expression(
        &mut self,
        session_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .evaluate(session_id, expression)?
                .as_bool()
                .unwrap_or(false)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Chromium page was not ready within {timeout:?}: {expression}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn capture_heap_snapshot(&mut self, session_id: &str, path: &Path) -> Result<()> {
        self.send_session(
            session_id,
            "HeapProfiler.enable",
            json!({}),
            REQUEST_TIMEOUT,
        )?;
        self.send_session(
            session_id,
            "HeapProfiler.collectGarbage",
            json!({}),
            CAPTURE_TIMEOUT,
        )?;
        let file =
            File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
        self.heap_capture = Some(HeapCapture { file, chunks: 0 });
        let result = self.send_session(
            session_id,
            "HeapProfiler.takeHeapSnapshot",
            json!({
                "reportProgress": false,
                "captureNumericValue": true,
            }),
            CAPTURE_TIMEOUT,
        );
        let mut capture = self
            .heap_capture
            .take()
            .context("Chromium heap capture state disappeared")?;
        capture
            .file
            .flush()
            .context("failed to flush Chromium heap snapshot")?;
        result?;
        if capture.chunks == 0 {
            bail!("Chromium heap snapshot emitted no chunks");
        }
        Ok(())
    }

    fn close_page(&mut self, page: OpenPage) -> Result<()> {
        self.primary_sessions.remove(&page.session_id);
        self.sessions.remove(&page.session_id);
        let closed = self.send_root(
            "Target.closeTarget",
            json!({"targetId": page.target_id}),
            REQUEST_TIMEOUT,
        );
        let disposed = self.send_root(
            "Target.disposeBrowserContext",
            json!({"browserContextId": page.browser_context_id}),
            REQUEST_TIMEOUT,
        );
        match (closed, disposed) {
            (Ok(_), Ok(_)) => Ok(()),
            (Err(error), Ok(_)) | (Ok(_), Err(error)) => Err(error),
            (Err(close), Err(dispose)) => {
                Err(close.context(format!("context disposal also failed: {dispose:#}")))
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("CDP request id overflowed")?;
        self.process.send(&json!({
            "id": id,
            "method": "Browser.close",
            "params": {},
        }))?;
        self.process.wait_for_exit()
    }

    fn terminate(&mut self) -> Result<()> {
        self.process.terminate()
    }

    fn check_fatal_error(&self) -> Result<()> {
        if let Some(error) = &self.fatal_error {
            bail!("{error}");
        }
        Ok(())
    }
}

enum CdpAction {
    Intercept {
        session_id: String,
        request_id: String,
        allowed: bool,
    },
    InitializeTarget {
        session_id: String,
        target_type: String,
    },
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("Chromium CDP value has no string {field}"))
}

fn protocol_error(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown Chromium CDP error");
    code.map_or_else(|| message.to_owned(), |code| format!("{message} ({code})"))
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChromiumCallFrame {
    #[serde(default)]
    function_name: String,
    #[serde(default)]
    url: String,
    line_number: i64,
    column_number: i64,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChromiumProfileNode {
    id: u64,
    call_frame: ChromiumCallFrame,
    #[serde(default)]
    children: Vec<u64>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChromiumProfile {
    nodes: Vec<ChromiumProfileNode>,
    #[serde(default)]
    samples: Vec<u64>,
    #[serde(default)]
    time_deltas: Vec<i64>,
}

fn parse_cpu_profile(value: &Value) -> Result<ChromiumProfile> {
    let profile: ChromiumProfile =
        serde_json::from_value(value.clone()).context("Chromium emitted an invalid CPU profile")?;
    if profile.nodes.is_empty()
        || profile.samples.is_empty()
        || profile.samples.len() != profile.time_deltas.len()
    {
        bail!("Chromium emitted an invalid CPU profile");
    }
    Ok(profile)
}

fn profile_graph(
    profile: &ChromiumProfile,
) -> (HashMap<u64, &ChromiumProfileNode>, HashMap<u64, u64>) {
    let nodes = profile.nodes.iter().map(|node| (node.id, node)).collect();
    let mut parents = HashMap::new();
    for node in &profile.nodes {
        for child in &node.children {
            parents.insert(*child, node.id);
        }
    }
    (nodes, parents)
}

fn target_nodes(profile: &ChromiumProfile, target_url: &str) -> Result<HashSet<u64>> {
    let (nodes, parents) = profile_graph(profile);
    let mut belongs = HashSet::new();
    for node in &profile.nodes {
        let mut current = Some(node.id);
        let mut visited = HashSet::new();
        while let Some(id) = current {
            if !visited.insert(id) {
                bail!("Chromium CPU profile contains a parent cycle");
            }
            let frame = nodes
                .get(&id)
                .context("Chromium CPU profile references a missing node")?;
            if frame.call_frame.url.starts_with(target_url) || belongs.contains(&id) {
                belongs.extend(visited);
                break;
            }
            current = parents.get(&id).copied();
        }
    }
    Ok(belongs)
}

fn cpu_active_milliseconds(profile: &ChromiumProfile, target_url: &str) -> Result<f64> {
    let target_nodes = target_nodes(profile, target_url)?;
    let duration = profile
        .samples
        .iter()
        .zip(&profile.time_deltas)
        .filter_map(|(node, delta)| {
            (*delta > 0 && target_nodes.contains(node)).then_some(*delta as u64)
        })
        .try_fold(0_u64, |total, delta| total.checked_add(delta))
        .context("Chromium CPU sample duration overflowed")?;
    if duration == 0 {
        bail!("Chromium CPU profile has no positive sample duration");
    }
    Ok(duration as f64 / 1_000.0)
}

fn parse_live_heap_bytes(path: &Path) -> Result<u64> {
    let snapshot: Value = serde_json::from_reader(BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    ))
    .context("Chromium emitted invalid heap JSON")?;
    let fields = snapshot
        .get("snapshot")
        .and_then(|snapshot| snapshot.get("meta"))
        .and_then(|meta| meta.get("node_fields"))
        .and_then(Value::as_array)
        .context("Chromium emitted an invalid V8 heap snapshot")?;
    let self_size = fields
        .iter()
        .position(|field| field.as_str() == Some("self_size"))
        .context("Chromium heap snapshot has no self_size field")?;
    let nodes = snapshot
        .get("nodes")
        .and_then(Value::as_array)
        .context("Chromium emitted an invalid V8 heap snapshot")?;
    if fields.is_empty() || nodes.is_empty() || nodes.len() % fields.len() != 0 {
        bail!("Chromium emitted an invalid V8 heap snapshot");
    }
    let mut total = 0_u64;
    for index in (self_size..nodes.len()).step_by(fields.len()) {
        let size = nodes[index]
            .as_u64()
            .context("Chromium heap snapshot contains an invalid node size")?;
        total = total
            .checked_add(size)
            .context("Chromium heap snapshot size overflowed")?;
    }
    if total == 0 {
        bail!("Chromium heap snapshot contains no live heap bytes");
    }
    Ok(total)
}

fn finish_capture_artifacts(
    artifacts: CaptureArtifacts,
    profile_source: &Value,
    profile: &ChromiumProfile,
    target_url: Option<&str>,
) -> Result<Vec<ArtifactEvidence>> {
    artifacts.write_cpu_profile(serde_json::to_vec(profile_source)?)?;
    let speedscope = chromium_speedscope(profile, target_url)?;
    artifacts.write_flamegraph(&speedscope)?;
    artifacts.finish()
}

fn chromium_speedscope(
    profile: &ChromiumProfile,
    target_url: Option<&str>,
) -> Result<SpeedscopeDocument> {
    let (nodes, parents) = profile_graph(profile);
    let target_nodes = target_url
        .map(|target_url| target_nodes(profile, target_url))
        .transpose()?;
    let mut builder = SpeedscopeBuilder::new("Chromium CPU", "bperf Playwright sidecar");
    let mut stack_cache = HashMap::<u64, Vec<usize>>::new();
    let mut samples = Vec::new();
    let mut weights = Vec::new();

    for (node_id, delta) in profile.samples.iter().zip(&profile.time_deltas) {
        if target_nodes
            .as_ref()
            .is_some_and(|target_nodes| !target_nodes.contains(node_id))
        {
            continue;
        }
        let stack = stack_for(
            *node_id,
            &nodes,
            &parents,
            &mut builder,
            &mut stack_cache,
            &mut HashSet::new(),
        )?;
        if stack.is_empty() {
            bail!("Chromium CPU profile contains an empty sampled stack");
        }
        samples.push(stack);
        weights.push((*delta).max(1) as f64);
    }
    if samples.is_empty() {
        bail!("Chromium CPU profile has no Speedscope samples");
    }
    builder.sampled_profile(
        "Chromium renderer JavaScript",
        "microseconds",
        0.0,
        samples,
        weights,
    )?;
    builder.finish()
}

fn stack_for(
    node_id: u64,
    nodes: &HashMap<u64, &ChromiumProfileNode>,
    parents: &HashMap<u64, u64>,
    builder: &mut SpeedscopeBuilder,
    cache: &mut HashMap<u64, Vec<usize>>,
    visiting: &mut HashSet<u64>,
) -> Result<Vec<usize>> {
    if let Some(stack) = cache.get(&node_id) {
        return Ok(stack.clone());
    }
    if !visiting.insert(node_id) {
        bail!("Chromium CPU profile contains a parent cycle");
    }
    let node = nodes
        .get(&node_id)
        .context("Chromium CPU profile references a missing node")?;
    let mut stack = if let Some(parent) = parents.get(&node_id) {
        stack_for(*parent, nodes, parents, builder, cache, visiting)?
    } else {
        Vec::new()
    };
    let frame = SpeedscopeFrame {
        name: if node.call_frame.function_name.is_empty() {
            "(anonymous)".to_owned()
        } else {
            node.call_frame.function_name.clone()
        },
        file: (!node.call_frame.url.is_empty()).then(|| node.call_frame.url.clone()),
        line: (node.call_frame.line_number >= 0).then_some(node.call_frame.line_number),
        col: (node.call_frame.column_number >= 0).then_some(node.call_frame.column_number),
    };
    stack.push(builder.frame(frame));
    visiting.remove(&node_id);
    cache.insert(node_id, stack.clone());
    Ok(stack)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::VecDeque,
        io::{Read as _, Write as _},
        net::{TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    use tempfile::tempdir;

    use crate::lab::{BrowserLab, Engine};

    use super::*;

    #[derive(Default)]
    struct ScriptedTransport {
        sent: Vec<Value>,
        incoming: RefCell<VecDeque<Value>>,
    }

    impl CdpTransport for ScriptedTransport {
        fn send(&mut self, message: &Value) -> Result<()> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn receive(&self, _timeout: Duration) -> Result<Value> {
            self.incoming
                .borrow_mut()
                .pop_front()
                .context("scripted CDP transport has no incoming message")
        }

        fn wait_for_exit(&mut self) -> Result<()> {
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("sidecar")
            .join("test")
            .join("fixtures")
            .join("captures")
            .join("chromium")
            .join(name)
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

    #[test]
    fn golden_chromium_capture_preserves_metrics_and_flamegraph_shape() {
        let profile: Value =
            serde_json::from_slice(&fs::read(fixture("cpu.json")).unwrap()).unwrap();
        let parsed = parse_cpu_profile(&profile).unwrap();
        assert_eq!(
            cpu_active_milliseconds(&parsed, "http://127.0.0.1:4317/").unwrap(),
            3.0
        );
        assert_eq!(parse_live_heap_bytes(&fixture("heap.json")).unwrap(), 96);
        let actual = serde_json::to_value(
            chromium_speedscope(&parsed, Some("http://127.0.0.1:4317/")).unwrap(),
        )
        .unwrap();
        let expected: Value =
            serde_json::from_slice(&fs::read(fixture("flamegraph.json")).unwrap()).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn malformed_chromium_captures_fail_explicitly() {
        assert!(parse_cpu_profile(&json!({})).is_err());
        let directory = tempdir().unwrap();
        let path = directory.path().join("heap.json");
        fs::write(&path, "{}").unwrap();
        assert!(parse_live_heap_bytes(&path).is_err());
        fs::write(
            &path,
            r#"{"snapshot":{"meta":{"node_fields":["self_size"]}},"nodes":[-1]}"#,
        )
        .unwrap();
        assert!(parse_live_heap_bytes(&path).is_err());
    }

    #[test]
    fn external_requests_are_failed_inside_the_target_session() {
        let transport = ScriptedTransport {
            incoming: RefCell::new(VecDeque::from([
                json!({
                    "sessionId": "page",
                    "method": "Fetch.requestPaused",
                    "params": {
                        "requestId": "request",
                        "request": {"url": "https://example.com/tracker.js"}
                    }
                }),
                json!({"id": 1, "result": {"product": "HeadlessChrome/149"}}),
            ])),
            ..ScriptedTransport::default()
        };
        let mut connection = CdpConnection::new(transport, PathBuf::from("downloads"));
        let response = connection
            .send_root("Browser.getVersion", json!({}), REQUEST_TIMEOUT)
            .unwrap();
        assert_eq!(response["product"], "HeadlessChrome/149");
        assert_eq!(
            connection.process.sent[1],
            json!({
                "id": 2,
                "sessionId": "page",
                "method": "Fetch.failRequest",
                "params": {
                    "requestId": "request",
                    "errorReason": "BlockedByClient"
                }
            })
        );
    }

    #[test]
    fn heap_snapshot_chunks_are_written_before_the_command_completes() {
        let heap = fs::read_to_string(fixture("heap.json")).unwrap();
        let transport = ScriptedTransport {
            incoming: RefCell::new(VecDeque::from([
                json!({"id": 1, "result": {}}),
                json!({"id": 2, "result": {}}),
                json!({
                    "sessionId": "page",
                    "method": "HeapProfiler.addHeapSnapshotChunk",
                    "params": {"chunk": heap}
                }),
                json!({"id": 3, "result": {}}),
            ])),
            ..ScriptedTransport::default()
        };
        let directory = tempdir().unwrap();
        let path = directory.path().join("heap.heapsnapshot");
        let mut connection = CdpConnection::new(transport, directory.path().join("downloads"));
        connection.capture_heap_snapshot("page", &path).unwrap();
        assert_eq!(parse_live_heap_bytes(&path).unwrap(), 96);
    }

    #[test]
    #[ignore = "launches the pinned Playwright Chromium browser"]
    fn browser_lab_uses_fresh_contexts_and_recovers_after_failure() {
        let server = FreshStateServer::start();
        let mut browser_lab = BrowserLab::start(RuntimeInstallation::discover().unwrap()).unwrap();

        let first = browser_lab
            .inspect_benchmark(Engine::Chromium, &server.url, None)
            .unwrap();
        let second = browser_lab
            .inspect_benchmark(Engine::Chromium, &server.url, None)
            .unwrap();
        assert_eq!(first.description["fresh"], true);
        assert_eq!(second.description["fresh"], true);

        browser_lab
            .inspect_benchmark(Engine::Chromium, "https://example.com/", None)
            .unwrap_err();

        let reopened = browser_lab
            .inspect_benchmark(Engine::Chromium, &server.url, None)
            .unwrap();
        assert_eq!(reopened.description["fresh"], true);
        browser_lab.finish().unwrap();
    }
}
