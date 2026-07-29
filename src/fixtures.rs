//! Acquisition and immutable locking of benchmark-owned browser resources.

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ureq::{Agent, ResponseExt, http::Uri};

use crate::project_modules::project_file;

const LOCK_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FixtureDescriptor {
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<FixtureResponse>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream: Option<FixtureStream>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureStream {
    chunk_size: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interval_ms: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FixtureLockEntry {
    descriptor: FixtureDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    final_url: Option<String>,
    body_path: PathBuf,
    sha256: String,
    size_bytes: u64,
    content_type: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FixtureLock {
    schema_version: u32,
    fixtures: Vec<FixtureLockEntry>,
}

#[derive(Debug)]
pub(crate) struct ResolvedFixtures {
    pub(crate) fixture_files: Vec<PathBuf>,
    pub(crate) fixture_lock: PathBuf,
}

pub(crate) struct LockedFixtures {
    entries: BTreeMap<String, LockedFixture>,
}

pub(crate) struct LockedFixture {
    entry: FixtureLockEntry,
    body: Arc<[u8]>,
}

#[derive(Clone, Copy)]
pub(crate) struct StreamDelivery {
    pub(crate) chunk_size: usize,
    pub(crate) interval_ms: u64,
}

impl LockedFixtures {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for entry in read_lock(path)? {
            let (entry, body) = validate_body(entry, "pinned benchmark fixture")?;
            let key = fixture_key(&entry.descriptor)?;
            if entries
                .insert(
                    key,
                    LockedFixture {
                        entry,
                        body: body.into(),
                    },
                )
                .is_some()
            {
                bail!("fixture lock contains duplicate descriptors");
            }
        }
        Ok(Self { entries })
    }

    pub(crate) fn find(&self, encoded_descriptor: &str) -> Option<&LockedFixture> {
        let descriptor = serde_json::from_str::<FixtureDescriptor>(encoded_descriptor).ok()?;
        let key = fixture_key(&descriptor).ok()?;
        self.entries.get(&key)
    }
}

impl LockedFixture {
    pub(crate) fn body(&self) -> Arc<[u8]> {
        Arc::clone(&self.body)
    }

    pub(crate) fn content_type(&self) -> &str {
        self.entry
            .descriptor
            .response
            .as_ref()
            .and_then(|response| response.content_type.as_deref())
            .unwrap_or(&self.entry.content_type)
    }

    pub(crate) fn stream(&self) -> Option<StreamDelivery> {
        self.entry
            .descriptor
            .response
            .as_ref()
            .and_then(|response| response.stream.as_ref())
            .map(|stream| StreamDelivery {
                chunk_size: stream.chunk_size,
                interval_ms: stream.interval_ms.unwrap_or(0),
            })
    }
}

pub(crate) fn resolve(
    root: &Path,
    benchmark: &Path,
    lock_path: &Path,
    cache_root: &Path,
    descriptors: &[FixtureDescriptor],
) -> Result<ResolvedFixtures> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve benchmark root {}", root.display()))?;
    let benchmark = project_file(&root, benchmark, "benchmark module")?;
    let benchmark_directory = benchmark
        .parent()
        .context("benchmark module has no parent directory")?;
    fs::create_dir_all(cache_root)
        .with_context(|| format!("failed to create fixture cache {}", cache_root.display()))?;
    let cache_root = fs::canonicalize(cache_root)
        .with_context(|| format!("failed to resolve fixture cache {}", cache_root.display()))?;

    let existing = read_lock(lock_path)?
        .into_iter()
        .map(|entry| fixture_key(&entry.descriptor).map(|key| (key, entry)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let agent = ureq::agent();
    let mut entries = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        validate_descriptor(descriptor)?;
        let key = fixture_key(descriptor)?;
        let pinned = existing
            .get(&key)
            .filter(|entry| {
                entry
                    .source_url
                    .as_ref()
                    .is_some_and(|source_url| !source_url.is_empty())
            })
            .cloned()
            .map(|entry| validate_body(entry, "pinned remote fixture").map(|(entry, _body)| entry))
            .transpose()?;
        entries.push(match pinned {
            Some(entry) => entry,
            None => acquire(descriptor, benchmark_directory, &root, &cache_root, &agent)?,
        });
    }

    let lock = FixtureLock {
        schema_version: LOCK_SCHEMA_VERSION,
        fixtures: entries,
    };
    let parent = lock_path
        .parent()
        .context("fixture lock has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create fixture lock directory {}",
            parent.display()
        )
    })?;
    let body = format!("{}\n", serde_json::to_string_pretty(&lock)?);
    if !fs::read(lock_path).is_ok_and(|existing| existing == body.as_bytes()) {
        bperf_storage::replace_file(lock_path, body.as_bytes())
            .with_context(|| format!("failed to write fixture lock {}", lock_path.display()))?;
    }
    let fixture_lock = fs::canonicalize(lock_path)
        .with_context(|| format!("failed to resolve fixture lock {}", lock_path.display()))?;
    let mut fixture_files = lock
        .fixtures
        .into_iter()
        .map(|entry| entry.body_path)
        .collect::<Vec<_>>();
    fixture_files.sort();
    Ok(ResolvedFixtures {
        fixture_files,
        fixture_lock,
    })
}

fn acquire(
    descriptor: &FixtureDescriptor,
    benchmark_directory: &Path,
    root: &Path,
    cache_root: &Path,
    agent: &Agent,
) -> Result<FixtureLockEntry> {
    let fallback_content_type = inferred_content_type(&descriptor.source);
    let (body, source_url, final_url, content_type) = if let Some(source_uri) =
        remote_uri(&descriptor.source)
    {
        let source_url = source_uri.to_string();
        let mut response = match agent.get(&source_url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => {
                bail!("remote fixture {source_url} returned HTTP {status}")
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to acquire remote fixture {source_url}"));
            }
        };
        let final_url = response.get_uri().to_string();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map_or_else(|| fallback_content_type.clone(), str::to_owned);
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .read_to_end(&mut body)
            .with_context(|| format!("failed to read remote fixture {source_url}"))?;
        (body, Some(source_url), Some(final_url), content_type)
    } else {
        let source = benchmark_directory.join(&descriptor.source);
        let source = project_file(root, &source, "fixture")?;
        let body = fs::read(&source)
            .with_context(|| format!("failed to read benchmark fixture {}", source.display()))?;
        (body, None, None, fallback_content_type)
    };

    let digest = format!("{:x}", Sha256::digest(&body));
    let body_path = cache_body(cache_root, &digest, &body)?;
    Ok(FixtureLockEntry {
        descriptor: descriptor.clone(),
        source_url,
        final_url,
        body_path,
        sha256: digest,
        size_bytes: body.len() as u64,
        content_type,
    })
}

fn cache_body(cache_root: &Path, digest: &str, body: &[u8]) -> Result<PathBuf> {
    let body_path = cache_root.join(digest);
    if !body_path.exists() {
        bperf_storage::publish_immutable(&body_path, body).with_context(|| {
            format!(
                "failed to persist benchmark fixture {}",
                body_path.display()
            )
        })?;
    }
    let cached = fs::read(&body_path)
        .with_context(|| format!("failed to read benchmark fixture {}", body_path.display()))?;
    if cached != body {
        bail!(
            "content-addressed fixture object is corrupt: {}",
            body_path.display()
        );
    }
    fs::canonicalize(&body_path).with_context(|| {
        format!(
            "failed to resolve benchmark fixture {}",
            body_path.display()
        )
    })
}

fn read_lock(path: &Path) -> Result<Vec<FixtureLockEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let lock: FixtureLock = serde_json::from_slice(
        &fs::read(path)
            .with_context(|| format!("failed to read fixture lock {}", path.display()))?,
    )
    .with_context(|| format!("invalid fixture lock {}", path.display()))?;
    if lock.schema_version != LOCK_SCHEMA_VERSION {
        bail!("unsupported fixture lock schema in {}", path.display());
    }
    for entry in &lock.fixtures {
        validate_descriptor(&entry.descriptor)?;
        if entry.content_type.trim().is_empty() {
            bail!("fixture lock contains an empty content type");
        }
    }
    Ok(lock.fixtures)
}

fn validate_body(mut entry: FixtureLockEntry, label: &str) -> Result<(FixtureLockEntry, Vec<u8>)> {
    let body = fs::read(&entry.body_path)
        .with_context(|| format!("{label} is missing: {}", entry.descriptor.source))?;
    if body.len() as u64 != entry.size_bytes
        || format!("{:x}", Sha256::digest(&body)) != entry.sha256
    {
        bail!("{label} is corrupt: {}", entry.descriptor.source);
    }
    entry.body_path = fs::canonicalize(&entry.body_path).with_context(|| {
        format!(
            "failed to resolve benchmark fixture {}",
            entry.body_path.display()
        )
    })?;
    Ok((entry, body))
}

fn validate_descriptor(descriptor: &FixtureDescriptor) -> Result<()> {
    if descriptor.source.trim().is_empty() {
        bail!("fixture descriptor contains an empty source");
    }
    if descriptor
        .response
        .as_ref()
        .and_then(|response| response.content_type.as_deref())
        .is_some_and(|content_type| content_type.trim().is_empty())
    {
        bail!("fixture descriptor contains an empty content type");
    }
    if descriptor
        .response
        .as_ref()
        .and_then(|response| response.stream.as_ref())
        .is_some_and(|stream| stream.chunk_size == 0)
    {
        bail!("fixture descriptor contains a zero stream chunk size");
    }
    Ok(())
}

fn fixture_key(descriptor: &FixtureDescriptor) -> Result<String> {
    serde_json::to_string(descriptor).context("failed to encode fixture descriptor")
}

fn remote_uri(source: &str) -> Option<Uri> {
    let uri = source.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    (uri.authority().is_some()
        && (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")))
    .then_some(uri)
}

fn inferred_content_type(source: &str) -> String {
    let path = source.parse::<Uri>().ok().map_or_else(
        || source.split(['?', '#']).next().unwrap_or(source).to_owned(),
        |uri| uri.path().to_owned(),
    );
    match Path::new(&path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("json") => "application/json",
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("mp4" | "m4s") => "video/mp4",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use serde_json::json;
    use tiny_http::{Header, Response, Server, StatusCode};

    use super::*;

    #[test]
    fn local_fixtures_are_reacquired_and_cannot_escape_the_project() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = project.path();
        let benchmark = root.join("sample.bench.ts");
        let source = root.join("fixtures").join("segment.txt");
        let lock = root.join(".bperf").join("fixture-lock.json");
        let cache = root.join(".bperf").join("objects");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&benchmark, "export default 1;\n").unwrap();
        fs::write(&source, b"first").unwrap();
        let descriptor: FixtureDescriptor =
            serde_json::from_value(json!({"source": "./fixtures/segment.txt"})).unwrap();

        let first = resolve(
            root,
            &benchmark,
            &lock,
            &cache,
            std::slice::from_ref(&descriptor),
        )
        .unwrap();
        assert_eq!(fs::read(&first.fixture_files[0]).unwrap(), b"first");
        fs::write(&source, b"second").unwrap();
        let second = resolve(root, &benchmark, &lock, &cache, &[descriptor]).unwrap();
        assert_eq!(fs::read(&second.fixture_files[0]).unwrap(), b"second");
        assert_ne!(first.fixture_files, second.fixture_files);

        let escaped: FixtureDescriptor = serde_json::from_value(json!({
            "source": outside.path().join("outside.txt"),
        }))
        .unwrap();
        fs::write(outside.path().join("outside.txt"), b"outside").unwrap();
        let error = resolve(root, &benchmark, &lock, &cache, &[escaped]).unwrap_err();
        assert!(format!("{error:#}").contains("fixture is outside benchmark root"));
    }

    #[test]
    fn remote_fixtures_follow_redirects_and_reuse_the_pinned_body() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let benchmark = root.join("sample.bench.ts");
        let lock = root.join(".bperf").join("fixture-lock.json");
        let cache = root.join(".bperf").join("objects");
        fs::write(&benchmark, "export default 1;\n").unwrap();

        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let address = server.server_addr().to_ip().unwrap();
        let server_thread =
            {
                let server = Arc::clone(&server);
                thread::spawn(move || {
                    let redirect = server.recv().unwrap();
                    assert_eq!(redirect.url(), "/redirect");
                    redirect
                        .respond(
                            Response::empty(StatusCode(302))
                                .with_header(Header::from_bytes("Location", "/asset.mp4").unwrap()),
                        )
                        .unwrap();
                    let asset = server.recv().unwrap();
                    assert_eq!(asset.url(), "/asset.mp4");
                    asset
                        .respond(Response::from_data(b"remote fixture".to_vec()).with_header(
                            Header::from_bytes("Content-Type", "video/custom").unwrap(),
                        ))
                        .unwrap();
                })
            };
        let source = format!("http://127.0.0.1:{}/redirect", address.port());
        let descriptor: FixtureDescriptor =
            serde_json::from_value(json!({"source": source})).unwrap();

        let first = resolve(
            root,
            &benchmark,
            &lock,
            &cache,
            std::slice::from_ref(&descriptor),
        )
        .unwrap();
        server_thread.join().unwrap();
        drop(server);
        assert_eq!(
            fs::read(&first.fixture_files[0]).unwrap(),
            b"remote fixture"
        );
        let lock_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
        assert_eq!(lock_value["fixtures"][0]["source_url"], source);
        assert!(
            lock_value["fixtures"][0]["final_url"]
                .as_str()
                .unwrap()
                .ends_with("/asset.mp4")
        );
        assert_eq!(lock_value["fixtures"][0]["content_type"], "video/custom");

        let second = resolve(
            root,
            &benchmark,
            &lock,
            &cache,
            std::slice::from_ref(&descriptor),
        )
        .unwrap();
        assert_eq!(first.fixture_files, second.fixture_files);
        fs::write(&second.fixture_files[0], b"corrupt").unwrap();
        let error = resolve(root, &benchmark, &lock, &cache, &[descriptor]).unwrap_err();
        assert!(format!("{error:#}").contains("pinned remote fixture is corrupt"));
    }
}
