//! Authenticated generation of the browser distribution registry embedded by bperf.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use flate2::read::GzDecoder;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};

const REGISTRY_SCHEMA_VERSION: u32 = 1;
const REGISTRY_PATH: &str = "crates/bperf-runtime/playwright-registry.json";
const PACKAGE_NAME: &str = "playwright-core";
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REGISTRY_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const NATIVE_DEPS_MODULE: &str = "\"packages/playwright-core/src/server/registry/nativeDeps.ts\"()";
const REGISTRY_MODULE: &str = "\"packages/playwright-core/src/server/registry/index.ts\"()";
const SELECTED_BROWSERS: [&str; 3] = ["chromium-headless-shell", "firefox", "webkit"];

struct NpmSigningKey {
    id: &'static str,
    spki: &'static str,
}

// npm signs packuments with registry-managed ECDSA P-256 keys. Pinning the
// public key prevents a compromised metadata response from introducing its own
// replacement trust root.
const NPM_SIGNING_KEYS: &[NpmSigningKey] = &[NpmSigningKey {
    id: "SHA256:DhQ8wR5APBvFHLF/+Tc+AYvPOdTpcIDqOhxsBHRwC7U",
    spki: "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEY6Ya7W++7aUPzvMTrezH6Ycx3c+HOKYCcNGybJZSCJq/fd7Qa8uuAKtdIkUQtQiEKERhAmE5lMMJhP8OkDOa2g==",
}];

pub(crate) fn command(mut arguments: impl Iterator<Item = OsString>) -> Result<()> {
    match arguments.next().as_deref().and_then(OsStr::to_str) {
        Some("update") => {
            let version = arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .context("usage: bperf-build playwright-registry update VERSION")?;
            if arguments.next().is_some() {
                bail!("usage: bperf-build playwright-registry update VERSION");
            }
            validate_version(&version)?;
            update(&version)
        }
        Some("check") => {
            if arguments.next().is_some() {
                bail!("usage: bperf-build playwright-registry check");
            }
            check()
        }
        _ => bail!("usage: bperf-build playwright-registry <update VERSION|check>"),
    }
}

fn update(version: &str) -> Result<()> {
    let path = registry_path()?;
    let generated = generate(version)?;
    if fs::read(&path).ok().as_deref() == Some(&generated) {
        println!(
            "Playwright {version} registry is already current at {}",
            path.display()
        );
        return Ok(());
    }
    fs::write(&path, generated)
        .with_context(|| format!("failed to write generated registry {}", path.display()))?;
    println!(
        "Updated the authenticated Playwright {version} registry at {}",
        path.display()
    );
    Ok(())
}

fn check() -> Result<()> {
    let path = registry_path()?;
    let existing = fs::read(&path)
        .with_context(|| format!("failed to read generated registry {}", path.display()))?;
    let version = version_from_registry(&existing)?;
    let generated = generate(&version)?;
    if existing != generated {
        bail!(
            "{} is stale; run `cargo run --locked -p bperf-build -- playwright-registry update {}`",
            path.display(),
            version
        );
    }
    println!(
        "Authenticated Playwright {} registry matches {}",
        version,
        path.display()
    );
    Ok(())
}

pub(crate) fn current_version() -> Result<String> {
    let path = registry_path()?;
    let registry = fs::read(&path)
        .with_context(|| format!("failed to read generated registry {}", path.display()))?;
    version_from_registry(&registry)
}

fn version_from_registry(registry: &[u8]) -> Result<String> {
    let identity: RegistryIdentity =
        serde_json::from_slice(registry).context("generated registry is invalid JSON")?;
    validate_version(&identity.source.version)?;
    Ok(identity.source.version)
}

fn registry_path() -> Result<PathBuf> {
    Ok(super::repository_root()?.join(REGISTRY_PATH))
}

fn validate_version(version: &str) -> Result<()> {
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        bail!("invalid Playwright package version {version:?}");
    }
    Ok(())
}

fn generate(version: &str) -> Result<Vec<u8>> {
    let package = authenticated_package(version)?;
    let sources = extract_registry_sources(&package.archive)?;
    let registry = compile_registry(package, sources)?;
    let mut output = serde_json::to_vec_pretty(&registry)?;
    output.push(b'\n');
    Ok(output)
}

#[derive(Deserialize)]
struct Packument {
    name: String,
    version: String,
    dist: PackageDistribution,
}

#[derive(Deserialize)]
struct PackageDistribution {
    integrity: String,
    tarball: String,
    signatures: Vec<PackageSignature>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PackageSignature {
    #[serde(rename = "keyid")]
    key_id: String,
    sig: String,
}

struct AuthenticatedPackage {
    archive: Vec<u8>,
    integrity: String,
    signature: PackageSignature,
    version: String,
}

fn authenticated_package(version: &str) -> Result<AuthenticatedPackage> {
    let metadata_url = format!("https://registry.npmjs.org/{PACKAGE_NAME}/{version}");
    let metadata = download_limited(&metadata_url, MAX_METADATA_BYTES)?;
    let packument: Packument =
        serde_json::from_slice(&metadata).context("npm package metadata is invalid JSON")?;
    if packument.name != PACKAGE_NAME || packument.version != version {
        bail!(
            "npm returned {}@{} while {}@{} was requested",
            packument.name,
            packument.version,
            PACKAGE_NAME,
            version
        );
    }
    let signature = verify_packument_signature(&packument)?;
    if !packument.dist.tarball.starts_with("https://") {
        bail!("npm package tarball URL is not HTTPS");
    }
    let archive = download_limited(&packument.dist.tarball, MAX_PACKAGE_BYTES)?;
    verify_integrity(&packument.dist.integrity, &archive)?;
    Ok(AuthenticatedPackage {
        archive,
        integrity: packument.dist.integrity,
        signature,
        version: version.to_owned(),
    })
}

fn download_limited(url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {url}"))?;
    if bytes.len() as u64 > limit {
        bail!("download from {url} exceeded the {limit}-byte limit");
    }
    Ok(bytes)
}

fn verify_packument_signature(packument: &Packument) -> Result<PackageSignature> {
    let message = format!(
        "{}@{}:{}",
        packument.name, packument.version, packument.dist.integrity
    );
    let mut matched_pinned_key = false;
    for key in NPM_SIGNING_KEYS {
        for signature in packument
            .dist
            .signatures
            .iter()
            .filter(|signature| signature.key_id == key.id)
        {
            matched_pinned_key = true;
            let Ok(signature_bytes) = BASE64.decode(&signature.sig) else {
                continue;
            };
            if verify_signature(key, message.as_bytes(), &signature_bytes).is_ok() {
                return Ok(signature.clone());
            }
        }
    }
    if matched_pinned_key {
        bail!("npm package signatures from pinned keys failed verification");
    }
    let key_ids = packument
        .dist
        .signatures
        .iter()
        .map(|signature| signature.key_id.as_str())
        .collect::<Vec<_>>();
    bail!("npm package has no signature from a pinned key (received {key_ids:?})")
}

fn verify_signature(key: &NpmSigningKey, message: &[u8], signature: &[u8]) -> Result<()> {
    let public_key = BASE64
        .decode(key.spki)
        .context("embedded npm signing key is invalid base64")?;
    let public_key = public_key
        .get(public_key.len().saturating_sub(65)..)
        .filter(|point| point.first() == Some(&4))
        .context("embedded npm signing key is not a P-256 public point")?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key)
        .verify(message, signature)
        .map_err(|_| anyhow::anyhow!("npm signature verification failed"))
}

fn verify_integrity(integrity: &str, archive: &[u8]) -> Result<()> {
    let encoded = integrity
        .strip_prefix("sha512-")
        .context("npm package integrity is not SHA-512")?;
    let expected = BASE64
        .decode(encoded)
        .context("npm package integrity is invalid base64")?;
    let actual = Sha512::digest(archive);
    if expected.as_slice() != actual.as_slice() {
        bail!("npm package tarball does not match its signed SHA-512 integrity");
    }
    Ok(())
}

struct RegistrySources {
    browsers_json: Vec<u8>,
    core_bundle: String,
}

fn extract_registry_sources(archive: &[u8]) -> Result<RegistrySources> {
    let decoder = GzDecoder::new(Cursor::new(archive));
    let mut tar = tar::Archive::new(decoder);
    let mut browsers_json = None;
    let mut core_bundle = None;
    for entry in tar.entries().context("invalid playwright-core tarball")? {
        let entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let destination = match path.as_path() {
            path if path == Path::new("package/browsers.json") => &mut browsers_json,
            path if path == Path::new("package/lib/coreBundle.js") => &mut core_bundle,
            _ => continue,
        };
        if destination.is_some() {
            bail!(
                "playwright-core tarball contains duplicate {}",
                path.display()
            );
        }
        let mut bytes = Vec::new();
        entry
            .take(MAX_REGISTRY_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_REGISTRY_SOURCE_BYTES {
            bail!("{} exceeds the source size limit", path.display());
        }
        *destination = Some(bytes);
    }
    Ok(RegistrySources {
        browsers_json: browsers_json.context("playwright-core has no browsers.json")?,
        core_bundle: String::from_utf8(
            core_bundle.context("playwright-core has no lib/coreBundle.js")?,
        )
        .context("playwright-core core bundle is not UTF-8")?,
    })
}

#[derive(Deserialize)]
struct BrowsersJson {
    browsers: Vec<BrowserDescriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserDescriptor {
    name: String,
    revision: String,
    #[serde(default)]
    revision_overrides: BTreeMap<String, String>,
    browser_version: Option<String>,
}

#[derive(Serialize)]
struct GeneratedRegistry {
    schema_version: u32,
    source: GeneratedSource,
    platforms: BTreeMap<String, BTreeMap<String, GeneratedArtifact>>,
    linux_dependencies: BTreeMap<String, GeneratedDependencyGroups>,
}

#[derive(Serialize)]
struct GeneratedSource {
    package: &'static str,
    version: String,
    integrity: String,
    signature: PackageSignature,
}

#[derive(Serialize)]
struct GeneratedArtifact {
    browser_version: String,
    revision: String,
    directory: String,
    executable: Vec<String>,
    download_path: String,
    mirrors: Vec<String>,
}

#[derive(Serialize)]
struct GeneratedDependencyGroups {
    tools: Vec<String>,
    chromium: Vec<String>,
    firefox: Vec<String>,
    webkit: Vec<String>,
}

#[derive(Deserialize)]
struct RegistryIdentity {
    source: ExistingSource,
}

#[derive(Deserialize)]
struct ExistingSource {
    version: String,
}

fn compile_registry(
    package: AuthenticatedPackage,
    sources: RegistrySources,
) -> Result<GeneratedRegistry> {
    let browsers: BrowsersJson =
        serde_json::from_slice(&sources.browsers_json).context("invalid browsers.json")?;
    let descriptors = browsers
        .browsers
        .into_iter()
        .map(|browser| (browser.name.clone(), browser))
        .collect::<BTreeMap<_, _>>();
    let playwright_mirrors = parse_assignment(
        &sources.core_bundle,
        REGISTRY_MODULE,
        "PLAYWRIGHT_CDN_MIRRORS",
    )?
    .into_string_array("PLAYWRIGHT_CDN_MIRRORS")?;
    let executable_paths =
        parse_assignment(&sources.core_bundle, REGISTRY_MODULE, "EXECUTABLE_PATHS")?
            .into_object("EXECUTABLE_PATHS")?;
    let download_paths = parse_assignment(&sources.core_bundle, REGISTRY_MODULE, "DOWNLOAD_PATHS")?
        .into_object("DOWNLOAD_PATHS")?;
    let cft = parse_cft_url(&sources.core_bundle)?;

    let mut platforms: BTreeMap<String, BTreeMap<String, GeneratedArtifact>> = BTreeMap::new();
    for name in SELECTED_BROWSERS {
        let descriptor = descriptors
            .get(name)
            .with_context(|| format!("browsers.json has no {name} descriptor"))?;
        let browser_version = descriptor
            .browser_version
            .as_deref()
            .with_context(|| format!("{name} has no browserVersion"))?;
        let browser_downloads = download_paths
            .get(name)
            .with_context(|| format!("DOWNLOAD_PATHS has no {name} entry"))?
            .as_object(name)?;
        let browser_executables = executable_paths
            .get(name)
            .with_context(|| format!("EXECUTABLE_PATHS has no {name} entry"))?
            .as_object(name)?;
        for (host, template) in browser_downloads {
            if matches!(template, JsValue::Null) || host == "<unknown>" {
                continue;
            }
            let revision = descriptor
                .revision_overrides
                .get(host)
                .unwrap_or(&descriptor.revision);
            let short_platform = short_platform(host)?;
            let executable = browser_executables
                .get(short_platform)
                .with_context(|| format!("{name} has no executable for {host} ({short_platform})"))?
                .as_string_array(&format!("{name} executable for {host}"))?;
            let (download_path, mirrors) = match template {
                JsValue::String(template) => {
                    (template.replace("%s", revision), playwright_mirrors.clone())
                }
                JsValue::CftUrl(suffix) => {
                    (cft.render(browser_version, suffix)?, cft.mirrors.clone())
                }
                _ => bail!("{name} download path for {host} is not static"),
            };
            let has_override = descriptor.revision_overrides.contains_key(host);
            let directory = if has_override {
                format!(
                    "{}-{revision}",
                    format!("{}_{}_special", name, host).replace('-', "_")
                )
            } else {
                format!("{}-{revision}", name.replace('-', "_"))
            };
            let artifact = GeneratedArtifact {
                browser_version: browser_version.to_owned(),
                revision: revision.clone(),
                directory,
                executable,
                download_path,
                mirrors,
            };
            if platforms
                .entry(host.clone())
                .or_default()
                .insert(name.to_owned(), artifact)
                .is_some()
            {
                bail!("duplicate {name} artifact for {host}");
            }
        }
    }
    for host in [
        "win64",
        "ubuntu24.04-x64",
        "ubuntu24.04-arm64",
        "mac15",
        "mac15-arm64",
    ] {
        let entries = platforms
            .get(host)
            .with_context(|| format!("generated registry has no {host} platform"))?;
        for name in SELECTED_BROWSERS {
            if !entries.contains_key(name) {
                bail!("generated registry has no {name} artifact for {host}");
            }
        }
    }

    let dependencies = parse_assignment(&sources.core_bundle, NATIVE_DEPS_MODULE, "deps")?
        .into_object("native dependency registry")?;
    let mut linux_dependencies = BTreeMap::new();
    for (host, value) in dependencies {
        let Some(platform) = host.strip_suffix("-x64") else {
            continue;
        };
        if !platform.starts_with("ubuntu") && !platform.starts_with("debian") {
            continue;
        }
        let groups = value.as_object(&format!("native dependencies for {host}"))?;
        let generated = GeneratedDependencyGroups {
            tools: dependency_group(groups, "tools", &host)?,
            chromium: dependency_group(groups, "chromium", &host)?,
            firefox: dependency_group(groups, "firefox", &host)?,
            webkit: dependency_group(groups, "webkit", &host)?,
        };
        linux_dependencies.insert(platform.to_owned(), generated);
    }
    for platform in [
        "ubuntu20.04",
        "ubuntu22.04",
        "ubuntu24.04",
        "ubuntu26.04",
        "debian11",
        "debian12",
        "debian13",
    ] {
        if !linux_dependencies.contains_key(platform) {
            bail!("generated registry has no dependency groups for {platform}");
        }
    }

    Ok(GeneratedRegistry {
        schema_version: REGISTRY_SCHEMA_VERSION,
        source: GeneratedSource {
            package: PACKAGE_NAME,
            version: package.version,
            integrity: package.integrity,
            signature: package.signature,
        },
        platforms,
        linux_dependencies,
    })
}

fn dependency_group(
    groups: &BTreeMap<String, JsValue>,
    name: &str,
    host: &str,
) -> Result<Vec<String>> {
    let packages = groups
        .get(name)
        .with_context(|| format!("{host} has no {name} dependency group"))?
        .as_string_array(&format!("{host} {name} dependencies"))?;
    if packages.is_empty() {
        bail!("{host} {name} dependency group is empty");
    }
    Ok(packages)
}

fn short_platform(host: &str) -> Result<&'static str> {
    if host == "win64" {
        return Ok("win-x64");
    }
    if host.starts_with("mac") {
        return Ok(if host.ends_with("-arm64") {
            "mac-arm64"
        } else {
            "mac-x64"
        });
    }
    if host.starts_with("ubuntu") || host.starts_with("debian") {
        return Ok(if host.ends_with("-arm64") {
            "linux-arm64"
        } else {
            "linux-x64"
        });
    }
    bail!("unsupported Playwright host platform {host}")
}

struct CftUrl {
    template: String,
    mirrors: Vec<String>,
}

impl CftUrl {
    fn render(&self, browser_version: &str, suffix: &str) -> Result<String> {
        let rendered = self
            .template
            .replace("${browserVersion}", browser_version)
            .replace("${suffix}", suffix);
        if rendered.contains("${") {
            bail!("unsupported substitution in cftUrl path template");
        }
        Ok(rendered)
    }
}

fn parse_cft_url(source: &str) -> Result<CftUrl> {
    let function = source
        .find("function cftUrl(")
        .context("core bundle has no cftUrl function")?;
    let source = &source[function..];
    let path = source
        .find("path:")
        .context("cftUrl has no path property")?;
    let mut parser = JsParser::new(&source[path + "path:".len()..]);
    let template = parser.parse_template_string()?;
    let mirrors = source
        .find("mirrors:")
        .context("cftUrl has no mirrors property")?;
    let mut parser = JsParser::new(&source[mirrors + "mirrors:".len()..]);
    let mirrors = parser.parse_value()?.into_string_array("cftUrl mirrors")?;
    if mirrors.is_empty() {
        bail!("cftUrl has no mirrors");
    }
    Ok(CftUrl { template, mirrors })
}

#[derive(Debug)]
enum JsValue {
    Null,
    String(String),
    Array(Vec<JsValue>),
    Object(BTreeMap<String, JsValue>),
    CftUrl(String),
}

impl JsValue {
    fn as_object(&self, description: &str) -> Result<&BTreeMap<String, JsValue>> {
        match self {
            Self::Object(value) => Ok(value),
            _ => bail!("{description} is not a static object"),
        }
    }

    fn into_object(self, description: &str) -> Result<BTreeMap<String, JsValue>> {
        match self {
            Self::Object(value) => Ok(value),
            _ => bail!("{description} is not a static object"),
        }
    }

    fn as_string_array(&self, description: &str) -> Result<Vec<String>> {
        match self {
            Self::Array(values) => values
                .iter()
                .map(|value| match value {
                    Self::String(value) => Ok(value.clone()),
                    _ => bail!("{description} contains a non-string value"),
                })
                .collect(),
            _ => bail!("{description} is not an array"),
        }
    }

    fn into_string_array(self, description: &str) -> Result<Vec<String>> {
        self.as_string_array(description)
    }
}

fn parse_assignment(source: &str, module: &str, name: &str) -> Result<JsValue> {
    let module = source
        .find(module)
        .with_context(|| format!("core bundle has no {module} module"))?;
    let source = &source[module..];
    let assignment = format!("{name} =");
    let offset = source
        .find(&assignment)
        .with_context(|| format!("core bundle module has no {assignment} assignment"))?;
    let mut parser = JsParser::new(&source[offset + assignment.len()..]);
    let value = parser.parse_value()?;
    parser.skip_trivia()?;
    parser.expect_byte(b';')?;
    Ok(value)
}

struct JsParser<'a> {
    source: &'a str,
    position: usize,
}

impl<'a> JsParser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            position: 0,
        }
    }

    fn parse_value(&mut self) -> Result<JsValue> {
        self.skip_trivia()?;
        match self.peek_byte() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"' | b'\'') => self.parse_string().map(JsValue::String),
            Some(byte) if is_identifier_start(byte) => {
                let identifier = self.parse_identifier()?;
                match identifier.as_str() {
                    "void" => {
                        self.skip_trivia()?;
                        self.expect_byte(b'0')?;
                        Ok(JsValue::Null)
                    }
                    "undefined" => Ok(JsValue::Null),
                    "cftUrl" => {
                        self.skip_trivia()?;
                        self.expect_byte(b'(')?;
                        let suffix = self.parse_string()?;
                        self.skip_trivia()?;
                        self.expect_byte(b')')?;
                        Ok(JsValue::CftUrl(suffix))
                    }
                    _ => bail!("unsupported static JavaScript value {identifier}"),
                }
            }
            other => bail!("unsupported static JavaScript token {other:?}"),
        }
    }

    fn parse_object(&mut self) -> Result<JsValue> {
        self.expect_byte(b'{')?;
        let mut values = BTreeMap::new();
        loop {
            self.skip_trivia()?;
            if self.consume_byte(b'}') {
                break;
            }
            let key = match self.peek_byte() {
                Some(b'"' | b'\'') => self.parse_string()?,
                Some(byte) if is_identifier_start(byte) => self.parse_identifier()?,
                other => bail!("unsupported static object key {other:?}"),
            };
            self.skip_trivia()?;
            self.expect_byte(b':')?;
            let value = self.parse_value()?;
            if values.insert(key.clone(), value).is_some() {
                bail!("duplicate static object key {key:?}");
            }
            self.skip_trivia()?;
            if self.consume_byte(b'}') {
                break;
            }
            self.expect_byte(b',')?;
        }
        Ok(JsValue::Object(values))
    }

    fn parse_array(&mut self) -> Result<JsValue> {
        self.expect_byte(b'[')?;
        let mut values = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.consume_byte(b']') {
                break;
            }
            values.push(self.parse_value()?);
            self.skip_trivia()?;
            if self.consume_byte(b']') {
                break;
            }
            self.expect_byte(b',')?;
        }
        Ok(JsValue::Array(values))
    }

    fn parse_string(&mut self) -> Result<String> {
        self.skip_trivia()?;
        let quote = self
            .next_byte()
            .filter(|quote| matches!(quote, b'"' | b'\''))
            .context("expected a JavaScript string")?;
        let mut value = String::new();
        loop {
            let byte = self.next_byte().context("unterminated JavaScript string")?;
            if byte == quote {
                break;
            }
            if byte == b'\\' {
                let escaped = self.next_byte().context("unterminated JavaScript escape")?;
                match escaped {
                    b'\\' => value.push('\\'),
                    b'\'' => value.push('\''),
                    b'"' => value.push('"'),
                    b'n' => value.push('\n'),
                    b'r' => value.push('\r'),
                    b't' => value.push('\t'),
                    b'b' => value.push('\u{0008}'),
                    b'f' => value.push('\u{000c}'),
                    b'v' => value.push('\u{000b}'),
                    b'0' => value.push('\0'),
                    b'x' => value.push(self.parse_hex_escape(2)?),
                    b'u' => value.push(self.parse_hex_escape(4)?),
                    b'\n' => {}
                    b'\r' => {
                        self.consume_byte(b'\n');
                    }
                    _ => bail!("unsupported JavaScript escape \\{}", escaped as char),
                }
                continue;
            }
            if byte.is_ascii() {
                value.push(byte as char);
            } else {
                self.position -= 1;
                let character = self.source[self.position..]
                    .chars()
                    .next()
                    .context("invalid UTF-8 character")?;
                value.push(character);
                self.position += character.len_utf8();
            }
        }
        Ok(value)
    }

    fn parse_template_string(&mut self) -> Result<String> {
        self.skip_trivia()?;
        self.expect_byte(b'`')?;
        let start = self.position;
        let mut escaped = false;
        while let Some(byte) = self.next_byte() {
            if byte == b'`' && !escaped {
                return Ok(self.source[start..self.position - 1].to_owned());
            }
            escaped = byte == b'\\' && !escaped;
            if byte != b'\\' {
                escaped = false;
            }
        }
        bail!("unterminated JavaScript template string")
    }

    fn parse_hex_escape(&mut self, digits: usize) -> Result<char> {
        let end = self
            .position
            .checked_add(digits)
            .context("JavaScript escape overflow")?;
        let value = self
            .source
            .get(self.position..end)
            .context("truncated JavaScript hex escape")?;
        self.position = end;
        let code = u32::from_str_radix(value, 16).context("invalid JavaScript hex escape")?;
        char::from_u32(code).context("invalid JavaScript Unicode scalar")
    }

    fn parse_identifier(&mut self) -> Result<String> {
        self.skip_trivia()?;
        let start = self.position;
        let first = self.next_byte().context("expected identifier")?;
        if !is_identifier_start(first) {
            bail!("invalid JavaScript identifier");
        }
        while self.peek_byte().is_some_and(is_identifier_continue) {
            self.position += 1;
        }
        Ok(self.source[start..self.position].to_owned())
    }

    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            while self
                .peek_byte()
                .is_some_and(|byte| byte.is_ascii_whitespace())
            {
                self.position += 1;
            }
            if self.remaining().starts_with("//") {
                self.position += 2;
                while self.peek_byte().is_some_and(|byte| byte != b'\n') {
                    self.position += 1;
                }
                continue;
            }
            if self.remaining().starts_with("/*") {
                let end = self.remaining()[2..]
                    .find("*/")
                    .context("unterminated JavaScript block comment")?;
                self.position += 2 + end + 2;
                continue;
            }
            return Ok(());
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<()> {
        self.skip_trivia()?;
        let actual = self.next_byte();
        if actual != Some(expected) {
            bail!(
                "expected JavaScript token {:?}, received {:?}",
                expected as char,
                actual.map(char::from)
            );
        }
        Ok(())
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        if self.peek_byte() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.source.as_bytes().get(self.position).copied()
    }

    fn next_byte(&mut self) -> Option<u8> {
        let byte = self.peek_byte()?;
        self.position += 1;
        Some(byte)
    }

    fn remaining(&self) -> &str {
        &self.source[self.position..]
    }
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_the_pinned_npm_signature() {
        let packument = Packument {
            name: PACKAGE_NAME.to_owned(),
            version: "1.61.1".to_owned(),
            dist: PackageDistribution {
                integrity: "sha512-h7Qlt6m4REp25qvIdvbDtVmD4LqVXfpRxhORv9L0jzETM05p4fuPJ3dKyuSXQxDSbXnmS79HAgi9589lGSpLkg==".to_owned(),
                tarball: "https://registry.npmjs.org/playwright-core/-/playwright-core-1.61.1.tgz".to_owned(),
                signatures: vec![PackageSignature {
                    key_id: NPM_SIGNING_KEYS[0].id.to_owned(),
                    sig: "MEQCIGgKFq2xelUn3NCMGeWH80siUz2btXfX97WQGyTnX5joAiAkR1FbL/uSNzM/8qew4c84kV2hTyv3URYnk2EE7Po+7w==".to_owned(),
                }],
            },
        };

        assert_eq!(
            verify_packument_signature(&packument).unwrap().key_id,
            NPM_SIGNING_KEYS[0].id
        );
    }

    #[test]
    fn parses_the_static_javascript_registry_subset() {
        let source = r#"
            // generated package source
            DOWNLOAD_PATHS = {
              "chromium-headless-shell": {
                "<unknown>": void 0,
                "win64": cftUrl("win64/chrome.zip"),
              },
              firefox: { win64: "builds/firefox/%s/firefox.zip" },
            };
        "#;
        let value = parse_assignment(source, "generated package source", "DOWNLOAD_PATHS")
            .unwrap()
            .into_object("downloads")
            .unwrap();
        let chromium = value["chromium-headless-shell"]
            .as_object("chromium")
            .unwrap();
        assert!(matches!(chromium["<unknown>"], JsValue::Null));
        assert!(matches!(
            &chromium["win64"],
            JsValue::CftUrl(value) if value == "win64/chrome.zip"
        ));
    }

    #[test]
    fn rejects_an_unpinned_signing_key() {
        let packument = Packument {
            name: PACKAGE_NAME.to_owned(),
            version: "1.61.1".to_owned(),
            dist: PackageDistribution {
                integrity: "sha512-value".to_owned(),
                tarball: "https://registry.npmjs.org/package.tgz".to_owned(),
                signatures: vec![PackageSignature {
                    key_id: "SHA256:untrusted".to_owned(),
                    sig: "invalid".to_owned(),
                }],
            },
        };

        assert!(
            verify_packument_signature(&packument)
                .unwrap_err()
                .to_string()
                .contains("no signature from a pinned key")
        );
    }

    #[test]
    fn rejects_package_bytes_that_do_not_match_signed_integrity() {
        let archive = b"authenticated playwright package";
        let integrity = format!("sha512-{}", BASE64.encode(Sha512::digest(archive)));

        verify_integrity(&integrity, archive).unwrap();
        assert!(
            verify_integrity(&integrity, b"tampered playwright package")
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
    }
}
