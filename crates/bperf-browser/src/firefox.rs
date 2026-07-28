//! Direct adapter for Playwright's pinned Firefox build.
//!
//! Juggler owns browser lifecycle and deterministic page execution. Firefox RDP
//! owns profiler and heap evidence. Neither protocol crosses the engine-neutral
//! browser laboratory boundary.

use std::{
    collections::{HashMap, HashSet},
    fs,
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
        location_contains_benchmark_code,
    },
    firefox_rdp::{FirefoxDebugSession, FirefoxHeapSnapshotFiles, free_port},
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
const FIREFOX_STARTUP_PREFERENCES: &str =
    "user_pref(\"extensions.systemAddon.update.url\", \"\");\n";

#[cfg(target_os = "linux")]
const FIREFOX_ENVIRONMENT_REMOVALS: &[&str] = &["SNAP_NAME", "SNAP_INSTANCE_NAME"];
#[cfg(not(target_os = "linux"))]
const FIREFOX_ENVIRONMENT_REMOVALS: &[&str] = &[];

#[derive(Clone)]
pub(crate) struct FirefoxAdapter {
    executable: PathBuf,
    revision: String,
    browser_version: String,
    playwright_version: String,
    executable_sha256: String,
}

impl EngineAdapter for FirefoxAdapter {
    type Lane = FirefoxLane;

    fn discover(installation: &RuntimeInstallation) -> Result<Self> {
        let firefox = installation.browser(BrowserName::Firefox)?;
        let executable = firefox_executable(firefox.directory())?;
        if !executable.is_file() {
            bail!(
                "Playwright Firefox revision {} is not installed at {}; run `npx playwright install firefox` for the pinned sidecar",
                firefox.revision(),
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
                    "Playwright Firefox executable is not executable: {}",
                    executable.display()
                );
            }
        }
        Ok(Self {
            executable_sha256: sha256_file(&executable)?,
            executable,
            revision: firefox.revision().to_owned(),
            browser_version: firefox.browser_version().to_owned(),
            playwright_version: installation.playwright_version().to_owned(),
        })
    }

    fn launch(&self) -> Result<Self::Lane> {
        FirefoxLane::launch(self)
    }
}

impl FirefoxAdapter {
    fn adapter_evidence(&self) -> AdapterEvidence {
        AdapterEvidence::Firefox {
            playwright: self.playwright_version.clone(),
            firefox_revision: self.revision.clone(),
            executable_sha256: self.executable_sha256.clone(),
            protocol_version: ADAPTER_PROTOCOL_VERSION,
            browser_workload_version: BROWSER_WORKLOAD_VERSION,
        }
    }
}

fn firefox_executable(browser_directory: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        Ok(browser_directory.join("firefox").join("firefox.exe"))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(browser_directory.join("firefox").join("firefox"))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(browser_directory
            .join("firefox")
            .join("Nightly.app")
            .join("Contents")
            .join("MacOS")
            .join("firefox"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = browser_directory;
        bail!("Playwright Firefox is unsupported on this platform")
    }
}

fn firefox_launch_arguments(root: &Path, rdp_port: u16) -> Vec<String> {
    vec![
        "-no-remote".to_owned(),
        "-headless".to_owned(),
        "-profile".to_owned(),
        root.join("profile").to_string_lossy().into_owned(),
        "-juggler-pipe".to_owned(),
        "--start-debugger-server".to_owned(),
        rdp_port.to_string(),
        "-silent".to_owned(),
    ]
}

fn prepare_firefox_profile(root: &Path) -> Result<()> {
    let profile = root.join("profile");
    fs::create_dir(&profile).context("failed to create the isolated Firefox profile")?;
    // Juggler waits for XPI startup work before quitting. A background system
    // add-on request can otherwise block process shutdown until its network
    // timeout.
    fs::write(profile.join("user.js"), FIREFOX_STARTUP_PREFERENCES)
        .context("failed to configure the isolated Firefox profile")
}

pub(crate) struct FirefoxLane {
    connection: JugglerConnection<BrowserProcess>,
    snapshots: FirefoxHeapSnapshotFiles,
    rdp_port: u16,
    browser: BrowserEvidence,
    adapter: AdapterEvidence,
    closed: bool,
}

impl FirefoxLane {
    fn launch(installation: &FirefoxAdapter) -> Result<Self> {
        let rdp_port = free_port()?;
        let process = BrowserProcess::spawn_configured(
            "bperf-firefox-",
            "rust-firefox",
            &installation.executable,
            FIREFOX_ENVIRONMENT_REMOVALS,
            |root| {
                prepare_firefox_profile(root)?;
                fs::create_dir(root.join("downloads"))
                    .context("failed to create the isolated Firefox download directory")?;
                Ok(firefox_launch_arguments(root, rdp_port))
            },
        )?;
        let launch_args = firefox_launch_arguments(process.working_directory(), rdp_port);
        let download_directory = process.working_directory().join("downloads");
        let root_pid = process.pid();
        let mut connection = JugglerConnection::new(process, download_directory);
        connection
            .send_root(
                "Browser.enable",
                json!({
                    "attachToDefaultContext": false,
                    "userPrefs": [
                        {
                            "name": "devtools.debugger.remote-enabled",
                            "value": true,
                        },
                        {
                            "name": "devtools.debugger.prompt-connection",
                            "value": false,
                        },
                    ],
                }),
                REQUEST_TIMEOUT,
            )
            .context("Firefox rejected Browser.enable")?;
        let info = connection
            .send_root("Browser.getInfo", json!({}), REQUEST_TIMEOUT)
            .context("Firefox rejected Browser.getInfo")?;
        let product = required_string(&info, "version")?;
        let actual_version = product
            .split_once('/')
            .map_or(product.as_str(), |(_, version)| version)
            .to_owned();
        if actual_version != installation.browser_version {
            bail!(
                "pinned Firefox revision {} reports version {}, expected {}",
                installation.revision,
                actual_version,
                installation.browser_version
            );
        }
        Ok(Self {
            connection,
            snapshots: FirefoxHeapSnapshotFiles::default(),
            rdp_port,
            browser: BrowserEvidence {
                root_pid,
                executable_path: installation.executable.to_string_lossy().into_owned(),
                version: actual_version,
                launch_args,
            },
            adapter: installation.adapter_evidence(),
            closed: false,
        })
    }

    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture> {
        let page = self.connection.open_page(&default_browser_config())?;
        let result = (|| {
            let artifacts = CaptureArtifacts::prepare(Engine::Firefox, artifact_directory)?;
            let mut debug = FirefoxDebugSession::connect(self.rdp_port)?;
            let anchor = decode_runtime_anchor(
                self.connection
                    .evaluate(&page.session_id, RUNTIME_ANCHOR_EXPRESSION)?,
            )
            .context("Firefox runtime anchor capture failed")?;
            debug.start_profiler()?;
            self.connection
                .evaluate(&page.session_id, DOCTOR_PROBE_EXPRESSION)?;
            let profile_source = debug.capture_profile()?;
            let profile = parse_profile(&profile_source)?;
            self.connection
                .evaluate(&page.session_id, SETTLE_EXPRESSION)?;
            let heap_path = artifacts.heap_snapshot_path();
            debug.capture_heap(&heap_path, &mut self.snapshots)?;
            let artifacts = finish_capture_artifacts(artifacts, &profile_source, &profile, None)?;
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
            let artifacts = CaptureArtifacts::prepare(Engine::Firefox, request.artifact_root)?;
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
                decode_batch_size(selected).context("Firefox batch calibration failed")?;

            let mut debug = FirefoxDebugSession::connect(self.rdp_port)?;
            debug.start_profiler()?;
            let workload = decode_workload(
                self.connection
                    .evaluate(&page.session_id, &script.execute(batch_size))?,
            )
            .context("Firefox workload execution failed")?;
            let profile_source = debug.capture_profile()?;
            let profile = parse_profile(&profile_source)?;
            let cpu_active_ms =
                cpu_active_milliseconds(&profile, request.target_url)? / f64::from(batch_size);
            self.connection
                .evaluate(&page.session_id, SETTLE_EXPRESSION)?;
            let heap_path = artifacts.heap_snapshot_path();
            let heap_bytes = debug.capture_heap(&heap_path, &mut self.snapshots)?;
            let artifacts = finish_capture_artifacts(
                artifacts,
                &profile_source,
                &profile,
                Some(request.target_url),
            )?;
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
                bail!("Firefox benchmark page returned no description");
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
        let browser = self.connection.close();
        let snapshots = self.snapshots.close();
        combine_shutdown(browser, snapshots)
    }

    fn terminate(&mut self) -> Result<()> {
        self.closed = true;
        let browser = self.connection.terminate();
        let snapshots = self.snapshots.close();
        combine_shutdown(browser, snapshots)
    }
}

impl EngineLane for FirefoxLane {
    fn probe(&mut self, artifact_directory: &Path) -> Result<ProbeCapture> {
        FirefoxLane::probe(self, artifact_directory)
    }

    fn measure_trial(&mut self, request: AdapterTrialRequest<'_>) -> Result<TrialCapture> {
        FirefoxLane::measure_trial(self, request)
    }

    fn inspect_benchmark(
        &mut self,
        target_url: &str,
        case_id: Option<&str>,
    ) -> Result<BenchmarkInspection> {
        FirefoxLane::inspect_benchmark(self, target_url, case_id)
    }

    fn close(&mut self) -> Result<()> {
        FirefoxLane::close(self)
    }

    fn terminate(&mut self) -> Result<()> {
        FirefoxLane::terminate(self)
    }
}

impl Drop for FirefoxLane {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.connection.terminate();
            let _ = self.snapshots.close();
            self.closed = true;
        }
    }
}

fn combine_page_close<T>(result: Result<T>, close: Result<()>) -> Result<T> {
    match (result, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(close_error)) => {
            Err(close_error.context("failed to close Firefox trial state"))
        }
        (Err(error), Err(close_error)) => Err(error.context(format!(
            "Firefox capture also failed to close its isolated context: {close_error:#}"
        ))),
    }
}

fn combine_shutdown(browser: Result<()>, snapshots: Result<()>) -> Result<()> {
    match (browser, snapshots) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(snapshot_error)) => Err(error.context(format!(
            "Firefox heap snapshot cleanup also failed: {snapshot_error:#}"
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
    ready: bool,
    main_frame_id: Option<String>,
    execution_contexts: HashMap<String, ExecutionContext>,
    load_fired: bool,
    navigation_error: Option<String>,
}

struct ExecutionContext {
    frame_id: Option<String>,
    name: Option<String>,
}

impl SessionState {
    fn main_execution_context(&self) -> Option<&str> {
        let frame_id = self.main_frame_id.as_deref()?;
        self.execution_contexts
            .iter()
            .find(|(_, context)| {
                context.frame_id.as_deref() == Some(frame_id)
                    && context.name.as_deref().is_none_or(str::is_empty)
            })
            .map(|(id, _)| id.as_str())
    }

    fn initialized(&self) -> bool {
        self.ready && self.main_frame_id.is_some() && self.main_execution_context().is_some()
    }
}

trait JugglerTransport {
    fn send(&mut self, message: &Value) -> Result<()>;
    fn receive(&self, timeout: Duration) -> Result<Value>;
    fn wait_for_exit(&mut self) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
}

impl JugglerTransport for BrowserProcess {
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

struct JugglerConnection<Transport: JugglerTransport> {
    process: Transport,
    download_directory: PathBuf,
    next_id: u64,
    responses: HashMap<u64, std::result::Result<Value, String>>,
    ignored_responses: HashSet<u64>,
    sessions: HashMap<String, SessionState>,
    target_sessions: HashMap<String, String>,
    fatal_error: Option<String>,
}

impl<Transport: JugglerTransport> JugglerConnection<Transport> {
    fn new(process: Transport, download_directory: PathBuf) -> Self {
        Self {
            process,
            download_directory,
            next_id: 1,
            responses: HashMap::new(),
            ignored_responses: HashSet::new(),
            sessions: HashMap::new(),
            target_sessions: HashMap::new(),
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
            .context("Firefox Juggler request id overflowed")?;
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
            .with_context(|| format!("failed to send Firefox Juggler command {method}"))?;
        self.wait_for_response(id, method, Instant::now() + timeout)
    }

    fn send_ignored(&mut self, session_id: &str, method: &str, params: Value) -> Result<()> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("Firefox Juggler request id overflowed")?;
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
                return response
                    .map_err(|message| anyhow::anyhow!("Firefox {method} failed: {message}"));
            }
            self.check_fatal_error()?;
            self.pump(deadline)?;
        }
    }

    fn pump(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("Firefox Juggler request timed out");
        }
        let message = self.process.receive(remaining)?;
        if let Some(action) = self.dispatch(message)? {
            self.handle_action(action)?;
        }
        Ok(())
    }

    fn dispatch(&mut self, message: Value) -> Result<Option<JugglerAction>> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let result = if let Some(error) = message.get("error") {
                Err(protocol_error(error))
            } else {
                Ok(message.get("result").cloned().unwrap_or_else(|| json!({})))
            };
            if self.ignored_responses.remove(&id) {
                if let Err(error) = result {
                    self.fatal_error = Some(format!(
                        "background Firefox Juggler command failed: {error}"
                    ));
                }
            } else {
                self.responses.insert(id, result);
            }
            return Ok(None);
        }

        let method = message
            .get("method")
            .and_then(Value::as_str)
            .context("Firefox Juggler event has no method")?;
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let session_id = message
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match method {
            "Browser.attachedToTarget" => {
                let session_id = required_string(&params, "sessionId")?;
                let target = params
                    .get("targetInfo")
                    .context("Firefox target attachment has no target info")?;
                if target.get("type").and_then(Value::as_str) != Some("page") {
                    bail!("Firefox attached an unsupported target type");
                }
                let target_id = required_string(target, "targetId")?;
                self.sessions.entry(session_id.clone()).or_default();
                self.target_sessions.insert(target_id, session_id);
                Ok(None)
            }
            "Browser.detachedFromTarget" => {
                let target_id = required_string(&params, "targetId")?;
                if let Some(session_id) = self.target_sessions.remove(&target_id) {
                    self.sessions.remove(&session_id);
                }
                Ok(None)
            }
            "Page.ready" => {
                self.session_state(session_id)?.ready = true;
                Ok(None)
            }
            "Page.frameAttached" => {
                if params.get("parentFrameId").is_none_or(Value::is_null) {
                    self.session_state(session_id)?.main_frame_id =
                        Some(required_string(&params, "frameId")?);
                }
                Ok(None)
            }
            "Page.navigationCommitted" => {
                let frame_id = required_string(&params, "frameId")?;
                let state = self.session_state(session_id)?;
                if state.main_frame_id.is_none() {
                    state.main_frame_id = Some(frame_id);
                }
                Ok(None)
            }
            "Page.navigationAborted" => {
                let frame_id = required_string(&params, "frameId")?;
                let state = self.session_state(session_id)?;
                if state.main_frame_id.as_deref() == Some(frame_id.as_str()) {
                    state.navigation_error = Some(
                        params
                            .get("errorText")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown navigation error")
                            .to_owned(),
                    );
                }
                Ok(None)
            }
            "Page.eventFired" => {
                if params.get("name").and_then(Value::as_str) == Some("load") {
                    let frame_id = required_string(&params, "frameId")?;
                    let state = self.session_state(session_id)?;
                    if state.main_frame_id.as_deref() == Some(frame_id.as_str()) {
                        state.load_fired = true;
                    }
                }
                Ok(None)
            }
            "Runtime.executionContextCreated" => {
                let context_id = required_string(&params, "executionContextId")?;
                let aux = params.get("auxData").unwrap_or(&Value::Null);
                let context = ExecutionContext {
                    frame_id: aux
                        .get("frameId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    name: aux.get("name").and_then(Value::as_str).map(str::to_owned),
                };
                self.session_state(session_id)?
                    .execution_contexts
                    .insert(context_id, context);
                Ok(None)
            }
            "Runtime.executionContextDestroyed" => {
                let context_id = required_string(&params, "executionContextId")?;
                self.session_state(session_id)?
                    .execution_contexts
                    .remove(&context_id);
                Ok(None)
            }
            "Runtime.executionContextsCleared" => {
                self.session_state(session_id)?.execution_contexts.clear();
                Ok(None)
            }
            "Page.crashed" => {
                self.fatal_error = Some("Firefox page crashed".to_owned());
                Ok(None)
            }
            "Network.requestWillBeSent" => {
                if params.get("isIntercepted").and_then(Value::as_bool) != Some(true) {
                    return Ok(None);
                }
                Ok(Some(JugglerAction::Intercept {
                    session_id: session_id
                        .context("Firefox network event has no target session")?,
                    request_id: required_string(&params, "requestId")?,
                    allowed: params
                        .get("url")
                        .and_then(Value::as_str)
                        .is_some_and(is_allowed_trial_url),
                }))
            }
            _ => Ok(None),
        }
    }

    fn session_state(&mut self, session_id: Option<String>) -> Result<&mut SessionState> {
        let session_id = session_id.context("Firefox page event has no target session")?;
        self.sessions
            .get_mut(&session_id)
            .with_context(|| format!("Firefox event referenced unknown session {session_id}"))
    }

    fn handle_action(&mut self, action: JugglerAction) -> Result<()> {
        match action {
            JugglerAction::Intercept {
                session_id,
                request_id,
                allowed,
            } => {
                let (method, params) = if allowed {
                    (
                        "Network.resumeInterceptedRequest",
                        json!({"requestId": request_id}),
                    )
                } else {
                    (
                        "Network.abortInterceptedRequest",
                        json!({
                            "requestId": request_id,
                            "errorCode": "NS_ERROR_FAILURE",
                        }),
                    )
                };
                self.send_ignored(&session_id, method, params)
            }
        }
    }

    fn open_page(&mut self, config: &BrowserTrialConfig) -> Result<OpenPage> {
        let created = self.send_root(
            "Browser.createBrowserContext",
            json!({"removeOnDetach": true}),
            REQUEST_TIMEOUT,
        )?;
        let browser_context_id = required_string(&created, "browserContextId")?;
        self.configure_context(&browser_context_id, config)?;
        let opened = self.send_root(
            "Browser.newPage",
            json!({"browserContextId": browser_context_id}),
            REQUEST_TIMEOUT,
        )?;
        let target_id = required_string(&opened, "targetId")?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        while self
            .target_sessions
            .get(&target_id)
            .and_then(|session_id| self.sessions.get(session_id))
            .is_none_or(|state| !state.initialized())
        {
            self.check_fatal_error()?;
            self.pump(deadline)?;
        }
        let session_id = self
            .target_sessions
            .get(&target_id)
            .cloned()
            .context("Firefox did not attach the new page target")?;
        self.wait_for_expression(&session_id, &installed_expression(), PAGE_READY_TIMEOUT)?;
        Ok(OpenPage {
            browser_context_id,
            target_id,
            session_id,
        })
    }

    fn configure_context(
        &mut self,
        browser_context_id: &str,
        config: &BrowserTrialConfig,
    ) -> Result<()> {
        let context = Value::String(browser_context_id.to_owned());
        self.send_root(
            "Browser.setInitScripts",
            json!({
                "browserContextId": context,
                "scripts": [{"script": bootstrap_source()}],
            }),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setDownloadOptions",
            json!({
                "browserContextId": context,
                "downloadOptions": {
                    "behavior": "cancel",
                    "downloadsDir": self.download_directory.to_string_lossy(),
                },
            }),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setDefaultViewport",
            json!({
                "browserContextId": context,
                "viewport": {
                    "viewportSize": {
                        "width": config.viewport.width,
                        "height": config.viewport.height,
                    },
                    "deviceScaleFactor": 1,
                    "isMobile": false,
                },
            }),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setLocaleOverride",
            json!({"browserContextId": context, "locale": config.locale}),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setTimezoneOverride",
            json!({
                "browserContextId": context,
                "timezoneId": config.timezone_id,
            }),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setExtraHTTPHeaders",
            json!({
                "browserContextId": context,
                "headers": [{"name": "Accept-Language", "value": config.locale}],
            }),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setColorScheme",
            json!({
                "browserContextId": context,
                "colorScheme": config.color_scheme,
            }),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setReducedMotion",
            json!({"browserContextId": context, "reducedMotion": "no-preference"}),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setForcedColors",
            json!({"browserContextId": context, "forcedColors": "none"}),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setContrast",
            json!({"browserContextId": context, "contrast": "no-preference"}),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setRequestInterception",
            json!({"browserContextId": context, "enabled": true}),
            REQUEST_TIMEOUT,
        )?;
        self.send_root(
            "Browser.setCacheDisabled",
            json!({"browserContextId": context, "cacheDisabled": true}),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    fn navigate(&mut self, session_id: &str, target_url: &str) -> Result<()> {
        if !is_allowed_adapter_url(target_url) {
            bail!("variant adapter returned a non-loopback URL: {target_url}");
        }
        let frame_id = {
            let state = self
                .sessions
                .get_mut(session_id)
                .context("cannot navigate an unknown Firefox page")?;
            state.load_fired = false;
            state.navigation_error = None;
            state
                .main_frame_id
                .clone()
                .context("Firefox page has no main frame")?
        };
        self.send_session(
            session_id,
            "Page.navigate",
            json!({"url": target_url, "frameId": frame_id}),
            REQUEST_TIMEOUT,
        )?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let state = self
                .sessions
                .get(session_id)
                .context("Firefox page disappeared during navigation")?;
            if let Some(error) = &state.navigation_error {
                bail!("Firefox navigation failed: {error}");
            }
            if state.load_fired {
                break;
            }
            self.check_fatal_error()?;
            self.pump(deadline)?;
        }
        self.wait_for_expression(session_id, &installed_expression(), PAGE_READY_TIMEOUT)
    }

    fn evaluate(&mut self, session_id: &str, expression: &str) -> Result<Value> {
        let execution_context_id = self
            .sessions
            .get(session_id)
            .and_then(SessionState::main_execution_context)
            .map(str::to_owned)
            .context("Firefox page has no main execution context")?;
        let evaluated = self.send_session(
            session_id,
            "Runtime.callFunction",
            json!({
                "executionContextId": execution_context_id,
                "functionDeclaration": format!(
                    "async function() {{ return await ({expression}); }}"
                ),
                "returnByValue": true,
                "args": [],
            }),
            CAPTURE_TIMEOUT,
        )?;
        if let Some(exception) = evaluated.get("exceptionDetails") {
            let description = exception
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| exception.get("value").map(Value::to_string))
                .unwrap_or_else(|| "JavaScript exception".to_owned());
            let stack = exception
                .get("stack")
                .and_then(Value::as_str)
                .unwrap_or_default();
            bail!(
                "Firefox page evaluation failed: {description}{}",
                if stack.is_empty() {
                    String::new()
                } else {
                    format!("\n{stack}")
                }
            );
        }
        let result = evaluated
            .get("result")
            .context("Firefox page evaluation returned no result")?;
        Ok(result.get("value").cloned().unwrap_or(Value::Null))
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
                bail!("Firefox page was not ready within {timeout:?}: {expression}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn close_page(&mut self, page: OpenPage) -> Result<()> {
        let closed = self.send_root(
            "Browser.removeBrowserContext",
            json!({"browserContextId": page.browser_context_id}),
            REQUEST_TIMEOUT,
        );
        self.target_sessions.remove(&page.target_id);
        self.sessions.remove(&page.session_id);
        closed.map(|_| ())
    }

    fn close(&mut self) -> Result<()> {
        self.process.send(&json!({
            "id": -9999,
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

enum JugglerAction {
    Intercept {
        session_id: String,
        request_id: String,
        allowed: bool,
    },
}

fn protocol_error(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("Firefox protocol response has no {field}"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeckoProfile {
    #[serde(default)]
    meta: GeckoMeta,
    #[serde(default)]
    pages: Vec<GeckoPage>,
    #[serde(default)]
    threads: Vec<GeckoThread>,
    #[serde(default)]
    processes: Vec<GeckoProfile>,
}

#[derive(Deserialize)]
struct GeckoPage {
    #[serde(rename = "innerWindowID")]
    inner_window_id: u64,
    url: String,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeckoMeta {
    interval: Option<f64>,
    process_type: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeckoThread {
    name: String,
    pid: Value,
    tid: Value,
    samples: GeckoSamples,
    stack_table: GeckoStackTable,
    frame_table: GeckoFrameTable,
    string_table: Vec<String>,
}

#[derive(Deserialize)]
struct GeckoSamples {
    schema: GeckoSampleSchema,
    data: Vec<Vec<Value>>,
}

#[derive(Deserialize)]
struct GeckoSampleSchema {
    stack: usize,
    time: usize,
}

#[derive(Deserialize)]
struct GeckoStackTable {
    schema: GeckoStackSchema,
    data: Vec<Vec<Value>>,
}

#[derive(Deserialize)]
struct GeckoStackSchema {
    prefix: usize,
    frame: usize,
}

#[derive(Deserialize)]
struct GeckoFrameTable {
    schema: GeckoFrameSchema,
    data: Vec<Vec<Value>>,
}

#[derive(Deserialize)]
struct GeckoFrameSchema {
    location: usize,
    #[serde(default, rename = "innerWindowID")]
    inner_window_id: Option<usize>,
}

fn parse_profile(source: &str) -> Result<GeckoProfile> {
    let value: Value =
        serde_json::from_str(source).context("Firefox emitted invalid Gecko Profiler JSON")?;
    if !value.get("threads").is_some_and(Value::is_array)
        || !value.get("processes").is_some_and(Value::is_array)
    {
        bail!("Firefox emitted an invalid Gecko Profiler document");
    }
    serde_json::from_value(value).context("Firefox emitted an invalid Gecko Profiler document")
}

fn cpu_active_milliseconds(profile: &GeckoProfile, target_url: &str) -> Result<f64> {
    fn process_duration(
        profile: &GeckoProfile,
        target_url: &str,
        root_interval: f64,
    ) -> Result<f64> {
        let interval = profile.meta.interval.unwrap_or(root_interval);
        let target_window_ids = target_window_ids(profile, target_url);
        let mut duration = 0.0;
        for thread in &profile.threads {
            let samples = thread_samples(thread)?;
            if samples.is_empty() {
                continue;
            }
            let timestamps = samples.iter().map(|sample| sample.time).collect::<Vec<_>>();
            let weights = positive_weights(&timestamps, interval)?;
            let mut cache = HashMap::new();
            for (sample, weight) in samples.iter().zip(weights) {
                if stack_belongs_to_target(
                    thread,
                    sample.stack_index,
                    target_url,
                    &target_window_ids,
                    &mut cache,
                    &mut HashSet::new(),
                )? {
                    duration += weight;
                }
            }
        }
        for child in &profile.processes {
            duration += process_duration(child, target_url, root_interval)?;
        }
        Ok(duration)
    }

    let duration = process_duration(profile, target_url, profile.meta.interval.unwrap_or(1.0))?;
    if !duration.is_finite() || duration <= 0.0 {
        bail!("Firefox CPU profile has no positive sample duration");
    }
    Ok(duration)
}

struct GeckoSample {
    stack_index: usize,
    time: f64,
}

fn thread_samples(thread: &GeckoThread) -> Result<Vec<GeckoSample>> {
    let mut samples = Vec::new();
    for row in &thread.samples.data {
        let Some(stack) = row.get(thread.samples.schema.stack).and_then(Value::as_f64) else {
            continue;
        };
        let Some(time) = row.get(thread.samples.schema.time).and_then(Value::as_f64) else {
            continue;
        };
        if !time.is_finite() {
            continue;
        }
        let stack_index =
            numeric_index(stack).context("Firefox CPU profile contains an invalid stack index")?;
        samples.push(GeckoSample { stack_index, time });
    }
    Ok(samples)
}

fn numeric_index(value: f64) -> Option<usize> {
    (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= usize::MAX as f64)
        .then_some(value as usize)
}

fn target_window_ids(profile: &GeckoProfile, target_url: &str) -> HashSet<u64> {
    // Unsymbolicated JIT frames can lose their URL while retaining the page's
    // innerWindowID. Location matching remains a fallback for older profiles
    // and external script frames that do not carry a page identifier.
    profile
        .pages
        .iter()
        .filter(|page| is_benchmark_code_url(&page.url) || page.url == target_url)
        .map(|page| page.inner_window_id)
        .collect()
}

fn stack_belongs_to_target(
    thread: &GeckoThread,
    stack_index: usize,
    target_url: &str,
    target_window_ids: &HashSet<u64>,
    cache: &mut HashMap<usize, bool>,
    visiting: &mut HashSet<usize>,
) -> Result<bool> {
    if let Some(result) = cache.get(&stack_index) {
        return Ok(*result);
    }
    if !visiting.insert(stack_index) {
        bail!("Firefox CPU profile contains a stack cycle");
    }
    let Some(stack_row) = thread.stack_table.data.get(stack_index) else {
        visiting.remove(&stack_index);
        return Ok(false);
    };
    let prefix = stack_row
        .get(thread.stack_table.schema.prefix)
        .and_then(Value::as_f64)
        .and_then(numeric_index);
    let frame_index = stack_row
        .get(thread.stack_table.schema.frame)
        .and_then(Value::as_f64)
        .and_then(numeric_index);
    let frame = frame_index.and_then(|index| thread.frame_table.data.get(index));
    let location = frame
        .and_then(|row| row.get(thread.frame_table.schema.location))
        .and_then(Value::as_f64)
        .and_then(numeric_index)
        .and_then(|index| thread.string_table.get(index));
    let inner_window_id = frame
        .and_then(|row| {
            thread
                .frame_table
                .schema
                .inner_window_id
                .and_then(|column| row.get(column))
        })
        .and_then(Value::as_u64);
    let result = inner_window_id.is_some_and(|id| target_window_ids.contains(&id))
        || location.is_some_and(|location| {
            location.contains(target_url)
                || location_contains_benchmark_code(location)
                || benchmark_worker_entry(location)
        })
        || prefix
            .map(|prefix| {
                stack_belongs_to_target(
                    thread,
                    prefix,
                    target_url,
                    target_window_ids,
                    cache,
                    visiting,
                )
            })
            .transpose()?
            .unwrap_or(false);
    visiting.remove(&stack_index);
    cache.insert(stack_index, result);
    Ok(result)
}

fn benchmark_worker_entry(location: &str) -> bool {
    location
        .strip_prefix("WorkerThreadPrimaryRunnable::Run ")
        .is_some_and(|script| script.starts_with('/') || location_contains_benchmark_code(script))
}

fn finish_capture_artifacts(
    artifacts: CaptureArtifacts,
    profile_source: &str,
    profile: &GeckoProfile,
    target_url: Option<&str>,
) -> Result<Vec<ArtifactEvidence>> {
    artifacts.write_cpu_profile(profile_source)?;
    let speedscope = firefox_speedscope(profile, target_url)?;
    artifacts.write_flamegraph(&speedscope)?;
    artifacts.finish()
}

fn firefox_speedscope(
    profile: &GeckoProfile,
    target_url: Option<&str>,
) -> Result<SpeedscopeDocument> {
    fn add_process(
        profile: &GeckoProfile,
        process_path: &[String],
        target_url: Option<&str>,
        root_interval: f64,
        builder: &mut SpeedscopeBuilder,
    ) -> Result<()> {
        let process_name = match profile.meta.process_type.as_ref() {
            Some(Value::Number(value)) if value.as_i64() == Some(0) => "Parent".to_owned(),
            Some(Value::String(value)) => value.clone(),
            Some(value) => value.to_string(),
            None => "Content".to_owned(),
        };
        let mut path = process_path.to_vec();
        path.push(process_name.clone());
        let label = path.join(" / ");
        let target_window_ids = target_url
            .map(|target_url| target_window_ids(profile, target_url))
            .unwrap_or_default();
        for thread in &profile.threads {
            let all_samples = thread_samples(thread)?;
            let mut stack_cache = HashMap::new();
            let mut target_cache = HashMap::new();
            let mut samples = Vec::new();
            let mut timestamps = Vec::new();
            for sample in all_samples {
                if let Some(target_url) = target_url
                    && !stack_belongs_to_target(
                        thread,
                        sample.stack_index,
                        target_url,
                        &target_window_ids,
                        &mut target_cache,
                        &mut HashSet::new(),
                    )?
                {
                    continue;
                }
                samples.push(speedscope_stack(
                    thread,
                    sample.stack_index,
                    &mut stack_cache,
                    builder,
                    &mut HashSet::new(),
                )?);
                timestamps.push(sample.time);
            }
            if samples.is_empty() {
                continue;
            }
            if samples.iter().any(Vec::is_empty) {
                bail!("Firefox CPU profile contains an empty sampled stack");
            }
            let interval = profile.meta.interval.unwrap_or(root_interval);
            let weights = positive_weights(&timestamps, interval)?;
            builder.sampled_profile(
                format!(
                    "{label} / {} ({}:{})",
                    thread.name,
                    display_json_scalar(&thread.pid),
                    display_json_scalar(&thread.tid)
                ),
                "milliseconds",
                timestamps[0],
                samples,
                weights,
            )?;
        }
        for child in &profile.processes {
            add_process(child, &path, target_url, root_interval, builder)?;
        }
        Ok(())
    }

    let mut builder = SpeedscopeBuilder::new("Firefox CPU", "bperf Playwright sidecar");
    add_process(
        profile,
        &[],
        target_url,
        profile.meta.interval.unwrap_or(1.0),
        &mut builder,
    )?;
    builder.finish()
}

fn speedscope_stack(
    thread: &GeckoThread,
    stack_index: usize,
    cache: &mut HashMap<usize, Vec<usize>>,
    builder: &mut SpeedscopeBuilder,
    visiting: &mut HashSet<usize>,
) -> Result<Vec<usize>> {
    if let Some(stack) = cache.get(&stack_index) {
        return Ok(stack.clone());
    }
    if !visiting.insert(stack_index) {
        bail!("Firefox CPU profile contains a stack cycle");
    }
    let row = thread
        .stack_table
        .data
        .get(stack_index)
        .context("Firefox CPU profile references a missing stack")?;
    let prefix = row
        .get(thread.stack_table.schema.prefix)
        .and_then(Value::as_f64)
        .and_then(numeric_index);
    let mut stack = if let Some(prefix) = prefix {
        speedscope_stack(thread, prefix, cache, builder, visiting)?
    } else {
        Vec::new()
    };
    let frame_index = row
        .get(thread.stack_table.schema.frame)
        .and_then(Value::as_f64)
        .and_then(numeric_index);
    if let Some(frame_index) = frame_index {
        let location = thread
            .frame_table
            .data
            .get(frame_index)
            .and_then(|row| row.get(thread.frame_table.schema.location))
            .and_then(Value::as_f64)
            .and_then(numeric_index)
            .and_then(|index| thread.string_table.get(index))
            .map_or("(unknown)", String::as_str);
        stack.push(builder.frame(SpeedscopeFrame::named(location)));
    }
    visiting.remove(&stack_index);
    cache.insert(stack_index, stack.clone());
    Ok(stack)
}

fn display_json_scalar(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque};

    use crate::{
        lab::{BrowserLab, Engine},
        test_support::FreshContextServer,
    };

    use super::*;

    #[derive(Default)]
    struct ScriptedTransport {
        sent: Vec<Value>,
        incoming: RefCell<VecDeque<Value>>,
    }

    impl JugglerTransport for ScriptedTransport {
        fn send(&mut self, message: &Value) -> Result<()> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn receive(&self, _timeout: Duration) -> Result<Value> {
            self.incoming
                .borrow_mut()
                .pop_front()
                .context("scripted Juggler transport has no incoming message")
        }

        fn wait_for_exit(&mut self) -> Result<()> {
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn checked_in_profile_preserves_cpu_duration_and_flamegraph_shape() {
        let source = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("sidecar/test/fixtures/captures/firefox/cpu.json"),
        )
        .unwrap();
        let profile = parse_profile(&source).unwrap();
        assert_eq!(
            cpu_active_milliseconds(&profile, "http://127.0.0.1:4317/").unwrap(),
            8.0
        );
        let actual = serde_json::to_value(
            firefox_speedscope(&profile, Some("http://127.0.0.1:4317/")).unwrap(),
        )
        .unwrap();
        let expected: Value = serde_json::from_slice(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join("sidecar/test/fixtures/captures/firefox/flamegraph.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn unresolved_jit_frames_use_their_page_identity_for_attribution() {
        let source = json!({
            "meta": {"interval": 1},
            "pages": [],
            "threads": [],
            "processes": [{
                "meta": {"interval": 1, "processType": 2},
                "pages": [{
                    "innerWindowID": 42,
                    "url": "http://127.0.0.1:4317/"
                }],
                "threads": [{
                    "name": "GeckoMain",
                    "pid": 10,
                    "tid": 11,
                    "samples": {
                        "schema": {"stack": 0, "time": 1},
                        "data": [[0, 0], [0, 2]]
                    },
                    "stackTable": {
                        "schema": {"prefix": 0, "frame": 1},
                        "data": [[null, 0]]
                    },
                    "frameTable": {
                        "schema": {"location": 0, "innerWindowID": 1},
                        "data": [[0, 42]]
                    },
                    "stringTable": ["js::RunScript"]
                }],
                "processes": []
            }]
        })
        .to_string();
        let profile = parse_profile(&source).unwrap();

        assert_eq!(
            cpu_active_milliseconds(&profile, "http://127.0.0.1:4317/").unwrap(),
            4.0
        );
        let speedscope = serde_json::to_value(
            firefox_speedscope(&profile, Some("http://127.0.0.1:4317/")).unwrap(),
        )
        .unwrap();
        assert_eq!(speedscope["profiles"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn malformed_gecko_profiles_fail_explicitly() {
        assert!(parse_profile("{}").is_err());
        assert!(parse_profile(r#"{"threads":[],"processes":false}"#).is_err());
        assert!(
            parse_profile(
                r#"{"threads":[{"name":"main","pid":1,"tid":1,"samples":{"schema":{"stack":0,"time":1},"data":[["bad",0]]},"stackTable":{"schema":{"prefix":0,"frame":1},"data":[]},"frameTable":{"schema":{"location":0},"data":[]},"stringTable":[]}],"processes":[]}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn gecko_worker_entry_attribution_accepts_only_benchmark_owned_scripts() {
        for location in [
            "WorkerThreadPrimaryRunnable::Run /worker.js",
            "WorkerThreadPrimaryRunnable::Run http://localhost:4317/worker.js",
            "WorkerThreadPrimaryRunnable::Run blob:http://127.0.0.1:4317/id",
        ] {
            assert!(benchmark_worker_entry(location), "{location}");
        }
        for location in [
            "WorkerThreadPrimaryRunnable::Run resource://gre/modules/worker.js",
            "WorkerThreadPrimaryRunnable::Run https://example.com/worker.js",
            "DOM Worker",
        ] {
            assert!(!benchmark_worker_entry(location), "{location}");
        }
    }

    #[test]
    fn launch_arguments_isolate_profile_and_expose_only_private_protocols() {
        let arguments = firefox_launch_arguments(Path::new("C:/tmp/lane"), 43210);
        assert_eq!(arguments[0..3], ["-no-remote", "-headless", "-profile"]);
        assert_eq!(
            arguments[3],
            Path::new("C:/tmp/lane").join("profile").to_string_lossy()
        );
        assert_eq!(
            arguments[4..],
            [
                "-juggler-pipe",
                "--start-debugger-server",
                "43210",
                "-silent",
            ]
        );
    }

    #[test]
    fn launch_profile_disables_background_system_addon_updates() {
        let root = tempfile::tempdir().unwrap();
        prepare_firefox_profile(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("profile").join("user.js")).unwrap(),
            FIREFOX_STARTUP_PREFERENCES
        );
    }

    #[test]
    fn external_requests_are_aborted_inside_the_target_session() {
        let transport = ScriptedTransport {
            incoming: RefCell::new(VecDeque::from([
                json!({
                    "sessionId": "page",
                    "method": "Network.requestWillBeSent",
                    "params": {
                        "requestId": "request",
                        "isIntercepted": true,
                        "url": "https://example.com/tracker.js",
                    }
                }),
                json!({"id": 1, "result": {"version": "Firefox/151.0"}}),
            ])),
            ..ScriptedTransport::default()
        };
        let mut connection = JugglerConnection::new(transport, PathBuf::from("downloads"));
        connection
            .sessions
            .insert("page".to_owned(), SessionState::default());
        let response = connection
            .send_root("Browser.getInfo", json!({}), REQUEST_TIMEOUT)
            .unwrap();
        assert_eq!(response["version"], "Firefox/151.0");
        assert_eq!(
            connection.process.sent[1],
            json!({
                "id": 2,
                "sessionId": "page",
                "method": "Network.abortInterceptedRequest",
                "params": {
                    "requestId": "request",
                    "errorCode": "NS_ERROR_FAILURE",
                },
            })
        );
    }

    #[test]
    #[ignore = "launches the pinned Playwright Firefox browser"]
    fn browser_lab_uses_fresh_contexts_and_recovers_after_failure() {
        let server = FreshContextServer::start();
        let mut browser_lab = BrowserLab::start(RuntimeInstallation::discover().unwrap()).unwrap();

        let first = browser_lab
            .inspect_benchmark(Engine::Firefox, server.url(), None)
            .unwrap();
        let second = browser_lab
            .inspect_benchmark(Engine::Firefox, server.url(), None)
            .unwrap();
        assert_eq!(first.description["fresh"], true);
        assert_eq!(second.description["fresh"], true);

        browser_lab
            .inspect_benchmark(Engine::Firefox, "https://example.com/", None)
            .unwrap_err();

        let reopened = browser_lab
            .inspect_benchmark(Engine::Firefox, server.url(), None)
            .unwrap();
        assert_eq!(reopened.description["fresh"], true);
        browser_lab.finish().unwrap();
    }
}
