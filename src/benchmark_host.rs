//! Loopback serving for one materialized managed benchmark.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use serde_json::json;
use tiny_http::{Header, Request, Response, ResponseBox, Server, StatusCode};

use crate::{
    fixtures::{LockedFixtures, StreamDelivery},
    project_modules::BrowserProjectBundle,
};

pub(crate) const PROTOCOL_VERSION: u32 = 2;

const BENCHMARK_ROUTE: &str = "/__bperf/benchmark.js";
const FIXTURE_ROUTE: &str = "/__bperf/fixture";
// Browsers normally open at most six HTTP/1.1 connections per origin. Two
// spare workers keep page and module requests responsive while fixtures stream.
const HOST_WORKERS: usize = 8;
const PAGE_DOCUMENT: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>bperf managed benchmark</title>
<script type="module">
const benchmark = await import("/__bperf/benchmark.js");
if (benchmark.default !== globalThis.__bperfDefinition) {
  throw new Error(
    "default export must be created with defineBrowserBenchmark",
  );
}
</script>"#;

pub(crate) struct AdapterOptions {
    pub(crate) root: PathBuf,
    pub(crate) benchmark: PathBuf,
    pub(crate) fixture_lock: PathBuf,
    pub(crate) bundle: PathBuf,
    pub(crate) bundle_metadata: PathBuf,
}

pub(crate) struct BenchmarkHost {
    origin: String,
    source_files: Vec<PathBuf>,
    server: Arc<Server>,
    shutdown: Arc<AtomicBool>,
    workers: Vec<JoinHandle<Result<()>>>,
}

impl BenchmarkHost {
    pub(crate) fn start(bundle: &BrowserProjectBundle, fixture_lock: &Path) -> Result<Self> {
        let content = Arc::new(HostedBenchmark::load(bundle, fixture_lock)?);
        let server = Arc::new(
            Server::http("127.0.0.1:0")
                .map_err(|error| anyhow!("failed to bind benchmark host: {error}"))?,
        );
        let address = server
            .server_addr()
            .to_ip()
            .context("benchmark host did not expose a TCP address")?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut workers: Vec<JoinHandle<Result<()>>> = Vec::with_capacity(HOST_WORKERS);
        for worker_id in 0..HOST_WORKERS {
            let worker_server = Arc::clone(&server);
            let worker_content = Arc::clone(&content);
            let worker_shutdown = Arc::clone(&shutdown);
            match thread::Builder::new()
                .name(format!("bperf-benchmark-host-{worker_id}"))
                .spawn(move || serve(worker_server, worker_content, worker_shutdown))
            {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    shutdown.store(true, Ordering::SeqCst);
                    for _ in &workers {
                        server.unblock();
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(error).context("failed to start benchmark host worker");
                }
            }
        }

        Ok(Self {
            origin: format!("http://127.0.0.1:{}/", address.port()),
            source_files: bundle.source_files().to_vec(),
            server,
            shutdown,
            workers,
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.origin
    }

    pub(crate) fn source_files(&self) -> &[PathBuf] {
        &self.source_files
    }

    pub(crate) fn close(mut self) -> Result<()> {
        self.stop();
        self.join()
    }

    fn wait(mut self) -> Result<()> {
        self.join()
    }

    fn stop(&mut self) {
        if !self.shutdown.swap(true, Ordering::SeqCst) {
            for _ in &self.workers {
                self.server.unblock();
            }
        }
    }

    fn join(&mut self) -> Result<()> {
        for worker in self.workers.drain(..) {
            worker
                .join()
                .map_err(|_| anyhow!("benchmark host worker panicked"))??;
        }
        Ok(())
    }
}

impl Drop for BenchmarkHost {
    fn drop(&mut self) {
        self.stop();
        let _ = self.join();
    }
}

pub(crate) fn run_adapter(options: AdapterOptions) -> Result<()> {
    let bundle = BrowserProjectBundle::open(
        &options.root,
        &options.benchmark,
        &options.bundle,
        &options.bundle_metadata,
    )?;
    let host = BenchmarkHost::start(&bundle, &options.fixture_lock)?;
    serde_json::to_writer(
        io::stdout(),
        &json!({
            "protocol_version": PROTOCOL_VERSION,
            "url": host.url(),
            "source_files": host.source_files(),
        }),
    )
    .context("failed to write benchmark host readiness")?;
    println!();
    io::stdout()
        .flush()
        .context("failed to flush benchmark host readiness")?;
    host.wait()
}

fn serve(
    server: Arc<Server>,
    content: Arc<HostedBenchmark>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        match server.recv() {
            Ok(request) if !shutdown.load(Ordering::SeqCst) => {
                respond(request, &content);
            }
            Ok(_) => return Ok(()),
            Err(_) if shutdown.load(Ordering::SeqCst) => return Ok(()),
            Err(error) => return Err(error).context("benchmark host stopped accepting requests"),
        }
    }
}

fn respond(request: Request, content: &HostedBenchmark) {
    let response = match content.response(&request) {
        Ok(response) => response,
        Err(error) => Response::from_string(format!("{error:#}"))
            .with_status_code(StatusCode(500))
            .boxed(),
    };
    if let Err(error) = request.respond(response) {
        eprintln!("[benchmark-host] failed to write response: {error}");
    }
}

struct HostedBenchmark {
    bundle: Arc<[u8]>,
    fixtures: LockedFixtures,
}

impl HostedBenchmark {
    fn load(bundle: &BrowserProjectBundle, fixture_lock: &Path) -> Result<Self> {
        let bundle_source = fs::read(bundle.bundle_file()).with_context(|| {
            format!(
                "failed to read browser bundle {}",
                bundle.bundle_file().display()
            )
        })?;
        Ok(Self {
            bundle: bundle_source.into(),
            fixtures: LockedFixtures::load(fixture_lock)?,
        })
    }

    fn response(&self, request: &Request) -> Result<ResponseBox> {
        let (path, query) = request
            .url()
            .split_once('?')
            .map_or((request.url(), ""), |(path, query)| (path, query));
        match path {
            "/" => static_response(
                StatusCode(200),
                PAGE_DOCUMENT.as_bytes().to_vec().into(),
                "text/html; charset=utf-8",
            ),
            BENCHMARK_ROUTE => static_response(
                StatusCode(200),
                Arc::clone(&self.bundle),
                "text/javascript; charset=utf-8",
            ),
            FIXTURE_ROUTE => self.fixture_response(request, query),
            _ => static_response(
                StatusCode(404),
                Arc::from(&b"Not found"[..]),
                "text/plain; charset=utf-8",
            ),
        }
    }

    fn fixture_response(&self, request: &Request, query: &str) -> Result<ResponseBox> {
        let descriptor = form_urlencoded::parse(query.as_bytes())
            .find_map(|(name, value)| (name == "descriptor").then_some(value.into_owned()));
        let Some(entry) = descriptor
            .as_deref()
            .and_then(|descriptor| self.fixtures.find(descriptor))
        else {
            return static_response(
                StatusCode(404),
                Arc::from(&b"Unknown benchmark fixture"[..]),
                "text/plain; charset=utf-8",
            );
        };

        let complete_body = entry.body();

        let range_header = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Range"))
            .map(|header| header.value.as_str());
        let range = match range_header.map(|value| byte_range(value, complete_body.len())) {
            Some(Ok(range)) => Some(range),
            Some(Err(())) => {
                return response(
                    StatusCode(416),
                    Arc::from(&b""[..]),
                    0..0,
                    vec![header(
                        "Content-Range",
                        &format!("bytes */{}", complete_body.len()),
                    )?],
                    None,
                );
            }
            None => None,
        };
        let selected = range.clone().unwrap_or(0..complete_body.len());
        let mut headers = vec![
            header("Accept-Ranges", "bytes")?,
            header("Cache-Control", "no-store")?,
            header("Content-Type", entry.content_type())?,
        ];
        if let Some(range) = &range {
            headers.push(header(
                "Content-Range",
                &format!(
                    "bytes {}-{}/{}",
                    range.start,
                    range.end - 1,
                    complete_body.len()
                ),
            )?);
        }
        response(
            if range.is_some() {
                StatusCode(206)
            } else {
                StatusCode(200)
            },
            complete_body,
            selected,
            headers,
            entry.stream(),
        )
    }
}

fn static_response(status: StatusCode, body: Arc<[u8]>, content_type: &str) -> Result<ResponseBox> {
    let length = body.len();
    response(
        status,
        body,
        0..length,
        vec![
            header("Cache-Control", "no-store")?,
            header("Content-Type", content_type)?,
        ],
        None,
    )
}

fn response(
    status: StatusCode,
    body: Arc<[u8]>,
    selected: std::ops::Range<usize>,
    headers: Vec<Header>,
    stream: Option<StreamDelivery>,
) -> Result<ResponseBox> {
    let length = selected.len();
    let reader = BodyReader {
        body,
        start: selected.start,
        position: selected.start,
        end: selected.end,
        chunk_size: stream.map_or(usize::MAX, |stream| stream.chunk_size),
        interval: Duration::from_millis(stream.map_or(0, |stream| stream.interval_ms)),
    };
    Ok(Response::new(status, headers, reader, Some(length), None).boxed())
}

fn header(name: &str, value: &str) -> Result<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .map_err(|()| anyhow!("invalid HTTP response header {name}: {value:?}"))
}

struct BodyReader {
    body: Arc<[u8]>,
    start: usize,
    position: usize,
    end: usize,
    chunk_size: usize,
    interval: Duration,
}

impl Read for BodyReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.end || output.is_empty() {
            return Ok(0);
        }
        let chunk_offset = (self.position - self.start) % self.chunk_size;
        if self.position > self.start && chunk_offset == 0 && !self.interval.is_zero() {
            thread::sleep(self.interval);
        }
        let length = output
            .len()
            .min(self.end - self.position)
            .min(self.chunk_size - chunk_offset);
        output[..length].copy_from_slice(&self.body[self.position..self.position + length]);
        self.position += length;
        Ok(length)
    }
}

fn byte_range(value: &str, size: usize) -> std::result::Result<std::ops::Range<usize>, ()> {
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

    let value = value.strip_prefix("bytes=").ok_or(())?;
    let (start, end) = value.split_once('-').ok_or(())?;
    if end.contains('-') || (start.is_empty() && end.is_empty()) || size == 0 {
        return Err(());
    }
    let parse = |value: &str| -> std::result::Result<u64, ()> {
        let value = value.parse::<u64>().map_err(|_| ())?;
        (value <= MAX_SAFE_INTEGER).then_some(value).ok_or(())
    };
    let (start, end) = if start.is_empty() {
        let suffix = usize::try_from(parse(end)?).map_err(|_| ())?.min(size);
        (size - suffix, size)
    } else {
        let start = usize::try_from(parse(start)?).map_err(|_| ())?;
        let end = if end.is_empty() {
            size
        } else {
            usize::try_from(parse(end)?)
                .map_err(|_| ())?
                .saturating_add(1)
                .min(size)
        };
        (start, end)
    };
    if start >= size || end <= start {
        return Err(());
    }
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpStream,
    };

    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn rust_host_serves_bundle_and_ranged_stream_fixture() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        let benchmark = root.join("sample.bench.ts");
        let bundle_file = root.join("browser-bundle.js");
        let metadata_file = root.join("browser-bundle.json");
        let body_file = root.join("fixture-body");
        let lock_file = root.join("fixture-lock.json");
        fs::write(&benchmark, "export default 42;\n").unwrap();
        fs::write(&bundle_file, "globalThis.bundleLoaded = true;\n").unwrap();
        fs::write(
            &metadata_file,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "bundler": { "name": "rolldown", "version": "test" },
                "entry_path": "sample.bench.ts",
                "source_files": ["sample.bench.ts"],
            }))
            .unwrap(),
        )
        .unwrap();
        let fixture = b"0123456789";
        fs::write(&body_file, fixture).unwrap();
        let descriptor = json!({
            "source": "./segment.txt",
            "response": {
                "contentType": "text/plain",
                "stream": { "chunkSize": 2, "intervalMs": 0 },
            },
        });
        fs::write(
            &lock_file,
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "fixtures": [{
                    "descriptor": descriptor,
                    "body_path": body_file,
                    "sha256": format!("{:x}", Sha256::digest(fixture)),
                    "size_bytes": fixture.len(),
                    "content_type": "application/octet-stream",
                }],
            }))
            .unwrap(),
        )
        .unwrap();

        let bundle =
            BrowserProjectBundle::open(root, &benchmark, &bundle_file, &metadata_file).unwrap();
        let host = BenchmarkHost::start(&bundle, &lock_file).unwrap();
        fs::write(&body_file, b"mutated after validation").unwrap();

        let page = request(host.url(), "GET / HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200"));
        assert!(page.contains("await import(\"/__bperf/benchmark.js\")"));
        assert!(!page.contains("importmap"));

        let bundle_response = request(
            host.url(),
            "GET /__bperf/benchmark.js HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
        assert!(bundle_response.starts_with("HTTP/1.1 200"));
        assert!(bundle_response.ends_with("globalThis.bundleLoaded = true;\n"));

        let key = serde_json::to_string(&descriptor).unwrap();
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("descriptor", &key)
            .finish();
        let fixture_response = request(
            host.url(),
            &format!(
                "GET {FIXTURE_ROUTE}?{query} HTTP/1.1\r\nRange: bytes=2-5\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(fixture_response.starts_with("HTTP/1.1 206"));
        assert!(fixture_response.contains("Content-Range: bytes 2-5/10"));
        assert!(fixture_response.ends_with("2345"));

        let head = request(
            host.url(),
            &format!("HEAD {FIXTURE_ROUTE}?{query} HTTP/1.1\r\nConnection: close\r\n\r\n"),
        );
        assert!(head.starts_with("HTTP/1.1 200"));
        assert!(head.ends_with("\r\n\r\n"));
        host.close().unwrap();
    }

    #[test]
    fn byte_ranges_match_the_fixture_contract() {
        assert_eq!(byte_range("bytes=2-5", 10), Ok(2..6));
        assert_eq!(byte_range("bytes=7-", 10), Ok(7..10));
        assert_eq!(byte_range("bytes=-3", 10), Ok(7..10));
        assert_eq!(byte_range("bytes=-20", 10), Ok(0..10));
        assert_eq!(byte_range("bytes=10-", 10), Err(()));
        assert_eq!(byte_range("items=0-1", 10), Err(()));
        assert_eq!(byte_range("bytes=0-1,4-5", 10), Err(()));
        assert_eq!(byte_range("bytes=-", 10), Err(()));
        assert_eq!(byte_range("bytes=0-1", 0), Err(()));
    }

    fn request(origin: &str, message: &str) -> String {
        let address = origin
            .strip_prefix("http://")
            .unwrap()
            .trim_end_matches('/');
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(message.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }
}
