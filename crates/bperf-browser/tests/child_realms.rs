use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use bperf_browser::lab::{
    ArtifactKind, BrowserLab, BrowserTrialConfig, BrowserTrialRequest, Engine, TrialBatchConfig,
    Viewport,
};
use bperf_runtime::installation::RuntimeInstallation;
use serde_json::json;
use tempfile::tempdir;

const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const EXPECTED_WORKER_VALUE: u32 = 667_023_402;
const EXPECTED_IFRAME_VALUE: u32 = 2_974_158_890;
const MAIN_DOCUMENT: &str = r#"<!doctype html>
<body><script>
const pending = new Map();
let nextId = 1;
const worker = new Worker("/worker.js");
const frame = document.createElement("iframe");
frame.src = `http://localhost:${location.port}/frame.html`;
document.body.append(frame);

const workerReady = new Promise(resolve => {
  worker.addEventListener("message", event => {
    if (event.data?.ready) return resolve();
    pending.get(`worker-${event.data.id}`)?.(event.data.value);
  });
});
const frameReady = new Promise(resolve => {
  addEventListener("message", event => {
    if (event.source !== frame.contentWindow) return;
    if (event.data?.ready) return resolve();
    pending.get(`frame-${event.data.id}`)?.(event.data.value);
  });
});

Promise.all([workerReady, frameReady]).then(() => {
  globalThis.__bperfDescription = { id: "child-realms", cases: [{ id: "run" }] };
  globalThis.__bperf = {
    async run(operation) {
      const id = nextId++;
      const workerResult = new Promise(resolve => pending.set(`worker-${id}`, resolve));
      const frameResult = new Promise(resolve => pending.set(`frame-${id}`, resolve));
      worker.postMessage({ id, seed: operation.seed });
      frame.contentWindow.postMessage({ id, seed: operation.seed }, "*");
      const [workerValue, frameValue] = await Promise.all([workerResult, frameResult]);
      pending.delete(`worker-${id}`);
      pending.delete(`frame-${id}`);
      return { workerValue, frameValue };
    },
  };
});
</script></body>"#;

const WORKER_SCRIPT: &str = r#"
const bperfWorkerHeap = [];
function bperfWorkerHotLoop(seed) {
  let value = seed >>> 0;
  for (let index = 0; index < 20_000_000; index += 1)
    value = Math.imul(value ^ index, 2654435761) >>> 0;
  bperfWorkerHeap.push(new Uint32Array(16_384).fill(value));
  return value;
}
onmessage = event => postMessage({
  id: event.data.id,
  value: bperfWorkerHotLoop(event.data.seed),
});
postMessage({ ready: true });
"#;

const FRAME_DOCUMENT: &str = r#"<!doctype html><script>
const bperfIframeHeap = [];
function bperfIframeHotLoop(seed) {
  let value = seed >>> 0;
  for (let index = 0; index < 20_000_000; index += 1)
    value = Math.imul(value ^ index, 2246822519) >>> 0;
  bperfIframeHeap.push(new Uint32Array(16_384).fill(value));
  return value;
}
addEventListener("message", event => {
  event.source.postMessage({
    id: event.data.id,
    value: bperfIframeHotLoop(event.data.seed),
  }, "*");
});
parent.postMessage({ ready: true }, "*");
</script>"#;

struct RealmServer {
    url: String,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RealmServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread = thread::spawn(move || {
            while thread_running.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if !thread_running.load(Ordering::Acquire) {
                            break;
                        }
                        let _ = respond(&mut stream);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
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

impl Drop for RealmServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://").trim_end_matches('/'));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn respond(stream: &mut TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    let request = read_request_headers(stream)?;
    let first_line = String::from_utf8_lossy(&request)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let (content_type, body) = if first_line.contains(" /worker.js ") {
        ("text/javascript", WORKER_SCRIPT)
    } else if first_line.contains(" /frame.html ") {
        ("text/html", FRAME_DOCUMENT)
    } else {
        ("text/html", MAIN_DOCUMENT)
    };
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;
    stream.shutdown(Shutdown::Write)
}

fn read_request_headers(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut request = Vec::with_capacity(1_024);
    let mut chunk = [0_u8; 1_024];
    loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed before sending complete HTTP headers",
            ));
        }
        request.extend_from_slice(&chunk[..count]);
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            return Ok(request);
        }
        if request.len() >= MAX_REQUEST_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request headers exceed the fixture limit",
            ));
        }
    }
}

fn gecko_thread_names(profile: &serde_json::Value, names: &mut Vec<String>) {
    names.extend(
        profile["threads"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|thread| thread["name"].as_str().map(str::to_owned)),
    );
    for process in profile["processes"].as_array().into_iter().flatten() {
        gecko_thread_names(process, names);
    }
}

fn gecko_worker_strings(profile: &serde_json::Value, strings: &mut Vec<String>) {
    for thread in profile["threads"].as_array().into_iter().flatten() {
        if thread["name"] == "DOM Worker" {
            strings.extend(
                thread["stringTable"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str())
                    .filter(|value| {
                        value.contains("worker")
                            || value.contains("Worker")
                            || value.contains("http")
                            || value.contains("bperf")
                    })
                    .map(str::to_owned),
            );
        }
    }
    for process in profile["processes"].as_array().into_iter().flatten() {
        gecko_worker_strings(process, strings);
    }
}

fn assert_child_realm_evidence(engine: Engine) {
    let server = RealmServer::start();
    let directory = tempdir().unwrap();
    let mut lab = BrowserLab::start(RuntimeInstallation::discover().unwrap()).unwrap();
    let browser = BrowserTrialConfig {
        viewport: Viewport {
            width: 1_280,
            height: 720,
        },
        locale: "en-US".to_owned(),
        timezone_id: "UTC".to_owned(),
        color_scheme: "light".to_owned(),
    };

    let root = directory.path().join(engine.as_str());
    let evidence = lab
        .measure_trial(BrowserTrialRequest {
            engine,
            artifact_root: &root,
            target_url: &server.url,
            operations: &[json!({"seed": 42})],
            browser: &browser,
            batches: TrialBatchConfig::SINGLE,
        })
        .unwrap();
    assert_eq!(
        evidence.workload.result,
        vec![json!({
            "workerValue": EXPECTED_WORKER_VALUE,
            "frameValue": EXPECTED_IFRAME_VALUE
        })],
        "{engine}"
    );

    let mut kinds_by_scope = BTreeMap::<&str, BTreeSet<ArtifactKind>>::new();
    let mut flamegraphs = String::new();
    let mut cpu_profiles = String::new();
    for artifact in &evidence.artifacts {
        kinds_by_scope
            .entry(&artifact.capture_scope)
            .or_default()
            .insert(artifact.kind);
        if artifact.kind == ArtifactKind::Flamegraph {
            flamegraphs.push_str(&fs::read_to_string(root.join(&artifact.path)).unwrap());
        } else if artifact.kind == ArtifactKind::CpuProfile {
            cpu_profiles.push_str(&fs::read_to_string(root.join(&artifact.path)).unwrap());
        }
    }
    assert!(kinds_by_scope.values().all(|kinds| kinds.len() == 3));
    match engine {
        Engine::Chromium => {
            assert!(kinds_by_scope.contains_key("page"));
            assert!(
                kinds_by_scope
                    .keys()
                    .any(|scope| scope.starts_with("worker-"))
            );
        }
        Engine::Firefox => {
            assert_eq!(
                kinds_by_scope.keys().copied().collect::<Vec<_>>(),
                ["browser-context"]
            );
        }
        Engine::Webkit => {
            assert!(kinds_by_scope.contains_key("page"));
            assert!(
                kinds_by_scope
                    .keys()
                    .any(|scope| scope.starts_with("worker-"))
            );
        }
    }
    let mut thread_names = Vec::new();
    let mut worker_strings = Vec::new();
    if engine == Engine::Firefox {
        let profile = serde_json::from_str(&cpu_profiles).unwrap();
        gecko_thread_names(&profile, &mut thread_names);
        gecko_worker_strings(&profile, &mut worker_strings);
    }
    let worker_visible = if engine == Engine::Firefox {
        flamegraphs.contains("WorkerThreadPrimaryRunnable::Run /worker.js")
    } else {
        flamegraphs.contains("bperfWorkerHotLoop")
    };
    assert!(
        worker_visible,
        "{engine}; raw profile marker: {}; threads: {thread_names:?}; worker strings: {worker_strings:?}",
        cpu_profiles.contains("bperfWorkerHotLoop"),
    );
    assert!(
        flamegraphs.contains("bperfIframeHotLoop"),
        "{engine}; raw profile marker: {}",
        cpu_profiles.contains("bperfIframeHotLoop")
    );
    lab.finish().unwrap();
}

#[test]
#[ignore = "launches the pinned Playwright Chromium browser"]
fn chromium_dedicated_workers_and_iframes_contribute_native_evidence() {
    assert_child_realm_evidence(Engine::Chromium);
}

#[test]
#[ignore = "launches the pinned Playwright Firefox browser"]
fn firefox_dedicated_workers_and_iframes_contribute_native_evidence() {
    assert_child_realm_evidence(Engine::Firefox);
}

#[test]
#[ignore = "launches the pinned Playwright WebKit browser"]
fn webkit_dedicated_workers_and_iframes_contribute_native_evidence() {
    assert_child_realm_evidence(Engine::Webkit);
}
