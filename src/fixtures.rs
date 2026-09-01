//! Acquisition and immutable locking of benchmark-owned browser resources.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ureq::{Agent, ResponseExt, http::Uri};

use crate::project_modules::project_file;

const LOCK_SCHEMA_VERSION: u32 = 1;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

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

#[derive(Debug)]
struct CachedBody {
    path: PathBuf,
    sha256: String,
    size_bytes: u64,
}

pub(crate) struct LockedFixtures {
    entries: BTreeMap<String, LockedFixture>,
}

pub(crate) struct LockedFixture {
    entry: FixtureLockEntry,
    body: tempfile::NamedTempFile,
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
            entries.insert(key, LockedFixture { entry, body });
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
    pub(crate) fn body(&self) -> Result<File> {
        self.body.reopen().with_context(|| {
            format!(
                "failed to open locked benchmark fixture {}",
                self.entry.descriptor.source
            )
        })
    }

    pub(crate) fn size(&self) -> Result<usize> {
        usize::try_from(self.entry.size_bytes).context("benchmark fixture is too large to serve")
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
            .map(|mut entry| {
                entry.body_path = cached_object_path(
                    &cache_root,
                    &entry.sha256,
                    &entry.body_path,
                    "pinned remote fixture",
                )?;
                validate_body(entry, "pinned remote fixture").map(|(entry, _body)| entry)
            })
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
    let (cached, source_url, final_url, content_type) = if let Some(source_uri) =
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
        let cached = cache_body(
            cache_root,
            response.body_mut().as_reader(),
            &format!("remote fixture {source_url}"),
        )?;
        (cached, Some(source_url), Some(final_url), content_type)
    } else {
        let source = benchmark_directory.join(&descriptor.source);
        let source = project_file(root, &source, "fixture")?;
        let body = File::open(&source)
            .with_context(|| format!("failed to read benchmark fixture {}", source.display()))?;
        let cached = cache_body(
            cache_root,
            body,
            &format!("benchmark fixture {}", source.display()),
        )?;
        (cached, None, None, fallback_content_type)
    };

    Ok(FixtureLockEntry {
        descriptor: descriptor.clone(),
        source_url,
        final_url,
        body_path: cached.path,
        sha256: cached.sha256,
        size_bytes: cached.size_bytes,
        content_type,
    })
}

fn cache_body(cache_root: &Path, mut source: impl Read, label: &str) -> Result<CachedBody> {
    let mut staged = tempfile::NamedTempFile::new_in(cache_root)
        .context("failed to stage a benchmark fixture")?;
    let (sha256, size_bytes) = copy_body(&mut source, &mut staged, label)?;
    let body_path = cache_root.join(&sha256);
    bperf_storage::publish_staged_immutable(&body_path, staged).with_context(|| {
        format!(
            "failed to persist benchmark fixture {}",
            body_path.display()
        )
    })?;
    let path = cached_object_path(
        cache_root,
        &sha256,
        &body_path,
        "content-addressed fixture object",
    )?;
    Ok(CachedBody {
        path,
        sha256,
        size_bytes,
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
    let mut descriptors = BTreeSet::new();
    for entry in &lock.fixtures {
        validate_descriptor(&entry.descriptor)?;
        if !descriptors.insert(fixture_key(&entry.descriptor)?) {
            bail!("fixture lock contains duplicate descriptors");
        }
        if entry.content_type.trim().is_empty() {
            bail!("fixture lock contains an empty content type");
        }
    }
    Ok(lock.fixtures)
}

fn validate_body(
    mut entry: FixtureLockEntry,
    label: &str,
) -> Result<(FixtureLockEntry, tempfile::NamedTempFile)> {
    let mut source = File::open(&entry.body_path)
        .with_context(|| format!("{label} is missing: {}", entry.descriptor.source))?;
    let mut body =
        tempfile::NamedTempFile::new().context("failed to stage a locked benchmark fixture")?;
    let (digest, size) = copy_body(
        &mut source,
        &mut body,
        &format!("{label}: {}", entry.descriptor.source),
    )?;
    body.flush()
        .context("failed to flush a locked benchmark fixture")?;
    if size != entry.size_bytes || digest != entry.sha256 {
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

fn copy_body(
    source: &mut impl Read,
    destination: &mut impl Write,
    label: &str,
) -> Result<(String, u64)> {
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut chunk = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let count = source
            .read(&mut chunk)
            .with_context(|| format!("failed to read {label}"))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .context("benchmark fixture size overflowed")?;
        digest.update(&chunk[..count]);
        destination
            .write_all(&chunk[..count])
            .with_context(|| format!("failed to stage {label}"))?;
    }
    Ok((format!("{:x}", digest.finalize()), size))
}

fn cached_object_path(
    cache_root: &Path,
    digest: &str,
    path: &Path,
    label: &str,
) -> Result<PathBuf> {
    if digest.len() != 64
        || !digest
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        bail!("{label} has an invalid content digest");
    }
    let resolved = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve {label} {}", path.display()))?;
    if resolved != cache_root.join(digest) {
        bail!("{label} is outside the content-addressed fixture cache");
    }
    Ok(resolved)
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
    fn fixture_locks_reject_duplicate_descriptors() {
        let directory = tempfile::tempdir().unwrap();
        let lock = directory.path().join("fixture-lock.json");
        let entry = json!({
            "descriptor": {"source": "fixture.txt"},
            "body_path": directory.path().join("unused"),
            "sha256": "0".repeat(64),
            "size_bytes": 0,
            "content_type": "application/octet-stream",
        });
        fs::write(
            &lock,
            serde_json::to_vec(&json!({
                "schema_version": LOCK_SCHEMA_VERSION,
                "fixtures": [entry.clone(), entry],
            }))
            .unwrap(),
        )
        .unwrap();

        let error = read_lock(&lock).err().expect("duplicate lock was accepted");
        assert!(error.to_string().contains("duplicate descriptors"));
    }

    #[test]
    fn locked_fixture_bodies_are_independent_disk_streams() {
        let directory = tempfile::tempdir().unwrap();
        let body_path = directory.path().join("body");
        let lock_path = directory.path().join("fixture-lock.json");
        let body = b"012345";
        let descriptor = json!({"source": "fixture.txt"});
        fs::write(&body_path, body).unwrap();
        fs::write(
            &lock_path,
            serde_json::to_vec(&json!({
                "schema_version": LOCK_SCHEMA_VERSION,
                "fixtures": [{
                    "descriptor": descriptor,
                    "body_path": body_path,
                    "sha256": format!("{:x}", Sha256::digest(body)),
                    "size_bytes": body.len(),
                    "content_type": "text/plain",
                }],
            }))
            .unwrap(),
        )
        .unwrap();

        let fixtures = LockedFixtures::load(&lock_path).unwrap();
        let key = serde_json::to_string(&descriptor).unwrap();
        let fixture = fixtures.find(&key).unwrap();
        fs::write(&body_path, b"mutated").unwrap();
        let mut first = fixture.body().unwrap();
        let mut second = fixture.body().unwrap();
        let mut prefix = [0_u8; 2];
        first.read_exact(&mut prefix).unwrap();
        let mut complete = Vec::new();
        second.read_to_end(&mut complete).unwrap();
        let mut remainder = Vec::new();
        first.read_to_end(&mut remainder).unwrap();

        assert_eq!(fixture.size().unwrap(), body.len());
        assert_eq!(&prefix, b"01");
        assert_eq!(complete, body);
        assert_eq!(remainder, b"2345");
    }

    #[test]
    fn fixture_cache_streams_source_with_bounded_reads() {
        struct GuardedReader {
            remaining: usize,
            largest_request: usize,
        }

        impl Read for GuardedReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                self.largest_request = self.largest_request.max(output.len());
                let count = output.len().min(self.remaining);
                output[..count].fill(b'x');
                self.remaining -= count;
                Ok(count)
            }
        }

        let cache = tempfile::tempdir().unwrap();
        let size = 2 * COPY_BUFFER_BYTES + 17;
        let mut source = GuardedReader {
            remaining: size,
            largest_request: 0,
        };

        let cached = cache_body(cache.path(), &mut source, "generated fixture").unwrap();

        assert_eq!(source.largest_request, COPY_BUFFER_BYTES);
        assert_eq!(cached.size_bytes, size as u64);
        assert_eq!(fs::metadata(cached.path).unwrap().len(), size as u64);
        assert_eq!(
            cached.sha256,
            format!("{:x}", Sha256::digest(vec![b'x'; size]))
        );
    }

    #[cfg(unix)]
    #[test]
    fn content_addressed_cache_objects_cannot_be_symlinks() {
        use std::os::unix::fs::symlink;

        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let body = b"outside";
        let digest = format!("{:x}", Sha256::digest(body));
        let outside_path = outside.path().join("body");
        fs::write(&outside_path, body).unwrap();
        symlink(&outside_path, cache.path().join(&digest)).unwrap();

        let error = cache_body(cache.path(), &body[..], "fixture").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the content-addressed fixture cache"),
            "unexpected error: {error:#}"
        );
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
