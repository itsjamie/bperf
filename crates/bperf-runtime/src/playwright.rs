//! Rust-native installation of the browser builds published for Playwright.
//!
//! bperf consumes Playwright's patched browser archives, but does not load or
//! execute the Playwright package. This module fixes the exact build registry,
//! translates it into one host-specific artifact, and installs that artifact
//! atomically into the conventional Playwright cache.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tempfile::tempdir_in;
use ureq::ResponseExt;
use zip::ZipArchive;

use crate::{
    installation::{BrowserName, InstalledBrowser},
    native_dependencies,
    registry::Registry,
};

const INSTALLATION_MARKER: &str = "INSTALLATION_COMPLETE";
const INSTALLATION_METADATA: &str = "BPERF_INSTALLATION";
const DOWNLOAD_REQUEST_ATTEMPTS: u32 = 3;
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 500_000;
const MAX_UNCOMPRESSED_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct PlaywrightInstallation {
    distribution: Registry,
    host: HostPlatform,
    browser_cache: PathBuf,
}

#[derive(Clone, Debug)]
struct HostPlatform {
    browser_artifact: String,
    native_dependencies: native_dependencies::Capability,
}

#[derive(Clone, Debug)]
struct BrowserArtifact {
    browser_version: String,
    directory: PathBuf,
    executable: PathBuf,
    name: BrowserName,
    revision: String,
    urls: Vec<String>,
}

impl PlaywrightInstallation {
    pub(crate) fn discover() -> Result<Self> {
        Ok(Self {
            distribution: Registry::embedded()?,
            host: playwright_host_platform()?,
            browser_cache: browser_registry_directory()?,
        })
    }

    pub(crate) fn browser(&self, name: BrowserName) -> Result<InstalledBrowser> {
        let artifact = self.artifact(name)?;
        Ok(InstalledBrowser {
            browser_version: artifact.browser_version,
            directory: artifact.directory,
            executable: artifact.executable,
            revision: artifact.revision,
        })
    }

    pub(crate) fn install(&self, browsers: &[BrowserName], with_dependencies: bool) -> Result<()> {
        let browsers = browsers.iter().copied().collect::<BTreeSet<_>>();
        if browsers.is_empty() {
            bail!("at least one browser must be selected for installation");
        }
        if with_dependencies {
            native_dependencies::install(
                &self.host.native_dependencies,
                &browsers,
                &self.distribution,
            )?;
        }
        fs::create_dir_all(&self.browser_cache).with_context(|| {
            format!(
                "failed to create browser registry {}",
                self.browser_cache.display()
            )
        })?;
        for browser in browsers {
            self.install_browser(self.artifact(browser)?)?;
        }
        Ok(())
    }

    pub(crate) fn version(&self) -> &str {
        self.distribution.version()
    }

    fn artifact(&self, name: BrowserName) -> Result<BrowserArtifact> {
        let descriptor = self
            .distribution
            .artifact(&self.host.browser_artifact, name.registry_name())?;
        let directory = self.browser_cache.join(&descriptor.directory);
        let executable = directory.join(descriptor.executable.iter().collect::<PathBuf>());
        let urls = download_urls(name, &descriptor.download_path, &descriptor.mirrors)?;
        Ok(BrowserArtifact {
            browser_version: descriptor.browser_version.clone(),
            directory,
            executable,
            name,
            revision: descriptor.revision.clone(),
            urls,
        })
    }

    fn install_browser(&self, artifact: BrowserArtifact) -> Result<()> {
        if artifact.executable.is_file() && artifact.directory.join(INSTALLATION_MARKER).is_file() {
            println!(
                "{} {} is already installed at {}",
                artifact.name.registry_name(),
                artifact.browser_version,
                artifact.directory.display()
            );
            return Ok(());
        }

        let temporary = tempdir_in(&self.browser_cache).with_context(|| {
            format!(
                "failed to create browser installation staging area in {}",
                self.browser_cache.display()
            )
        })?;
        let archive_path = temporary.path().join("browser.zip");
        let source_url = download(&artifact, &archive_path)?;
        let staged = temporary.path().join("browser");
        fs::create_dir(&staged)?;
        extract_zip(&archive_path, &staged)?;

        let relative_executable = artifact
            .executable
            .strip_prefix(&artifact.directory)
            .context("browser executable escaped its installation directory")?;
        let staged_executable = staged.join(relative_executable);
        if !staged_executable.is_file() {
            bail!(
                "{} archive did not contain its expected executable {}",
                artifact.name.registry_name(),
                relative_executable.display()
            );
        }
        ensure_executable(&staged_executable)?;
        write_installation_receipt(
            &staged,
            &format!(
                "provider=playwright\nprovider_version={}\nbrowser={}\nbrowser_version={}\nrevision={}\nhost={}\nsource={source_url}\n",
                self.version(),
                artifact.name.registry_name(),
                artifact.browser_version,
                artifact.revision,
                self.host.browser_artifact,
            ),
        )?;

        if artifact.directory.exists() {
            let incomplete = artifact.directory.with_extension("incomplete");
            if incomplete.exists() {
                fs::remove_dir_all(&incomplete).with_context(|| {
                    format!(
                        "failed to remove stale browser staging directory {}",
                        incomplete.display()
                    )
                })?;
            }
            fs::rename(&artifact.directory, &incomplete).with_context(|| {
                format!(
                    "failed to quarantine incomplete browser installation {}",
                    artifact.directory.display()
                )
            })?;
            if let Err(error) = fs::rename(&staged, &artifact.directory) {
                let _ = fs::rename(&incomplete, &artifact.directory);
                return Err(error).with_context(|| {
                    format!(
                        "failed to activate browser installation {}",
                        artifact.directory.display()
                    )
                });
            }
            fs::remove_dir_all(&incomplete).with_context(|| {
                format!(
                    "failed to remove replaced browser installation {}",
                    incomplete.display()
                )
            })?;
        } else if let Err(error) = fs::rename(&staged, &artifact.directory) {
            if artifact.executable.is_file()
                && artifact.directory.join(INSTALLATION_MARKER).is_file()
            {
                return Ok(());
            }
            return Err(error).with_context(|| {
                format!(
                    "failed to activate browser installation {}",
                    artifact.directory.display()
                )
            });
        }

        println!(
            "Installed {} {} at {}",
            artifact.name.registry_name(),
            artifact.browser_version,
            artifact.directory.display()
        );
        Ok(())
    }
}

fn write_installation_receipt(directory: &Path, metadata: &str) -> Result<()> {
    File::create_new(directory.join(INSTALLATION_MARKER))
        .context("browser archive contains the reserved installation marker")?;
    File::create_new(directory.join(INSTALLATION_METADATA))
        .context("browser archive contains the reserved installation metadata")?
        .write_all(metadata.as_bytes())?;
    Ok(())
}

fn download_urls(name: BrowserName, path: &str, default_mirrors: &[String]) -> Result<Vec<String>> {
    let specific_override = match name {
        BrowserName::ChromiumHeadlessShell => "PLAYWRIGHT_CHROMIUM_DOWNLOAD_HOST",
        BrowserName::Firefox => "PLAYWRIGHT_FIREFOX_DOWNLOAD_HOST",
        BrowserName::Webkit => "PLAYWRIGHT_WEBKIT_DOWNLOAD_HOST",
    };
    let (override_host, override_name) = match nonempty_env(specific_override) {
        Some(host) => (Some(host), specific_override),
        None => (
            nonempty_env("PLAYWRIGHT_DOWNLOAD_HOST"),
            "PLAYWRIGHT_DOWNLOAD_HOST",
        ),
    };
    download_urls_from(
        path,
        default_mirrors,
        override_host.as_deref(),
        override_name,
    )
}

fn download_urls_from(
    path: &str,
    default_mirrors: &[String],
    override_host: Option<&str>,
    override_name: &str,
) -> Result<Vec<String>> {
    let mirrors = if let Some(host) = override_host {
        vec![(host, override_name)]
    } else {
        default_mirrors
            .iter()
            .map(|mirror| (mirror.as_str(), "embedded browser mirror"))
            .collect()
    };
    mirrors
        .into_iter()
        .map(|(mirror, label)| DownloadSource::parse(mirror, label).map(|source| source.join(path)))
        .collect()
}

#[derive(Clone, Debug)]
struct DownloadSource {
    base_url: String,
}

impl DownloadSource {
    fn parse(value: &str, label: &str) -> Result<Self> {
        let uri = validate_https_url(value, label)?;
        if uri
            .path_and_query()
            .is_some_and(|path| path.query().is_some())
        {
            bail!("{label} must not contain a query string");
        }
        Ok(Self {
            base_url: value.trim_end_matches('/').to_owned(),
        })
    }

    fn join(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }
}

fn validate_https_url(value: &str, label: &str) -> Result<ureq::http::Uri> {
    let uri = value
        .parse::<ureq::http::Uri>()
        .with_context(|| format!("{label} is not a valid URL"))?;
    if uri.scheme_str() != Some("https") || uri.authority().is_none() {
        bail!("{label} must be an absolute HTTPS URL");
    }
    Ok(uri)
}

fn retry_request<T>(
    mut request: impl FnMut() -> std::result::Result<T, ureq::Error>,
) -> std::result::Result<T, ureq::Error> {
    let mut attempt = 1;
    loop {
        match request() {
            Err(error)
                if attempt < DOWNLOAD_REQUEST_ATTEMPTS && retryable_request_error(&error) =>
            {
                std::thread::sleep(Duration::from_secs(u64::from(attempt)));
                attempt += 1;
            }
            result => return result,
        }
    }
}

fn retryable_request_error(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::StatusCode(408 | 429 | 500 | 502 | 503 | 504)
    )
}

fn download(artifact: &BrowserArtifact, destination: &Path) -> Result<String> {
    let agent =
        ureq::Agent::new_with_config(ureq::Agent::config_builder().https_only(true).build());
    let mut failures = Vec::new();
    for url in &artifact.urls {
        print!(
            "Downloading {} {} from {url} ... ",
            artifact.name.registry_name(),
            artifact.browser_version
        );
        io::stdout().flush().ok();
        let result = (|| -> Result<String> {
            let mut response = retry_request(|| agent.get(url).call())
                .with_context(|| format!("request failed for {url}"))?;
            let final_url = response.get_uri().to_string();
            validate_https_url(&final_url, "browser download redirect target")?;
            let mut output = File::create(destination)?;
            let mut reader = response.body_mut().as_reader().take(MAX_ARCHIVE_BYTES + 1);
            let downloaded = io::copy(&mut reader, &mut output)
                .with_context(|| format!("failed to download {url}"))?;
            if downloaded > MAX_ARCHIVE_BYTES {
                bail!("browser archive exceeds the compressed size limit");
            }
            output.sync_all()?;
            Ok(final_url)
        })();
        match result {
            Ok(final_url) => {
                println!("done");
                return Ok(final_url);
            }
            Err(error) => {
                println!("failed");
                failures.push(format!("{url}: {error:#}"));
            }
        }
    }
    bail!(
        "failed to download {} {}:\n{}",
        artifact.name.registry_name(),
        artifact.browser_version,
        failures.join("\n")
    )
}

fn extract_zip(archive_path: &Path, destination: &Path) -> Result<()> {
    let archive_file = File::open(archive_path)
        .with_context(|| format!("failed to open {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(archive_file)
        .with_context(|| format!("invalid browser archive {}", archive_path.display()))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("browser archive contains too many entries");
    }
    let mut total_size = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        total_size = total_size
            .checked_add(entry.size())
            .context("browser archive size overflow")?;
        if total_size > MAX_UNCOMPRESSED_BYTES {
            bail!("browser archive exceeds the uncompressed size limit");
        }
        let relative = entry
            .enclosed_name()
            .with_context(|| format!("unsafe browser archive path {}", entry.name()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            set_mode(&target, entry.unix_mode())?;
            continue;
        }
        if entry.is_symlink() {
            install_symlink(&mut entry, destination, &relative)?;
            continue;
        }
        fs::create_dir_all(
            target
                .parent()
                .context("browser archive entry has no parent")?,
        )?;
        let mut output = File::create(&target)
            .with_context(|| format!("failed to create {}", target.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("failed to extract {}", target.display()))?;
        set_mode(&target, entry.unix_mode())?;
    }
    Ok(())
}

#[cfg(unix)]
fn install_symlink(entry: &mut impl Read, root: &Path, relative: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    let mut value = String::new();
    entry.read_to_string(&mut value)?;
    let link = Path::new(&value);
    if link.is_absolute() {
        bail!("browser archive symlink has an absolute target");
    }
    let parent = relative
        .parent()
        .context("browser archive symlink has no parent")?;
    validate_relative_target(parent, link)?;
    let target = root.join(relative);
    fs::create_dir_all(
        target
            .parent()
            .context("browser archive symlink has no parent")?,
    )?;
    symlink(link, &target).with_context(|| format!("failed to create symlink {}", target.display()))
}

#[cfg(not(unix))]
fn install_symlink(_entry: &mut impl Read, _root: &Path, relative: &Path) -> Result<()> {
    bail!(
        "browser archive contains unsupported symlink {}",
        relative.display()
    )
}

#[cfg(any(unix, test))]
fn validate_relative_target(parent: &Path, link: &Path) -> Result<()> {
    let mut depth = parent.components().count();
    for component in link.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if depth > 0 => depth -= 1,
            std::path::Component::ParentDir => {
                bail!("browser archive symlink escapes its installation")
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                bail!("browser archive symlink target is not relative")
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::metadata(path)?;
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o755))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn browser_registry_directory() -> Result<PathBuf> {
    let configured =
        nonempty_env("BPERF_BROWSERS_PATH").or_else(|| nonempty_env("PLAYWRIGHT_BROWSERS_PATH"));
    let path = match configured.as_deref() {
        Some("0") => std::env::current_exe()?
            .parent()
            .context("bperf executable has no parent directory")?
            .join("bperf-browsers"),
        Some(value) => {
            let configured = PathBuf::from(value);
            if configured.is_absolute() {
                configured
            } else {
                nonempty_env_path("INIT_CWD")
                    .unwrap_or(std::env::current_dir()?)
                    .join(configured)
            }
        }
        None => {
            #[cfg(windows)]
            {
                nonempty_env_path("LOCALAPPDATA")
                    .or_else(|| {
                        nonempty_env_path("USERPROFILE")
                            .map(|home| home.join("AppData").join("Local"))
                    })
                    .context("cannot locate the Windows browser cache")?
                    .join("ms-playwright")
            }
            #[cfg(target_os = "macos")]
            {
                home_directory()?
                    .join("Library")
                    .join("Caches")
                    .join("ms-playwright")
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                nonempty_env_path("XDG_CACHE_HOME")
                    .unwrap_or(home_directory()?.join(".cache"))
                    .join("ms-playwright")
            }
        }
    };
    Ok(path)
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(unix)]
fn home_directory() -> Result<PathBuf> {
    nonempty_env_path("HOME").context("cannot locate the browser cache because HOME is unset")
}

fn playwright_host_platform() -> Result<HostPlatform> {
    let mut host = detected_host_platform()?;
    if let Some(overridden) =
        std::env::var_os("PLAYWRIGHT_HOST_PLATFORM_OVERRIDE").filter(|value| !value.is_empty())
    {
        host.browser_artifact = overridden
            .into_string()
            .map_err(|_| anyhow::anyhow!("PLAYWRIGHT_HOST_PLATFORM_OVERRIDE is not UTF-8"))?;
    }
    Ok(host)
}

fn detected_host_platform() -> Result<HostPlatform> {
    #[cfg(windows)]
    {
        if !cfg!(target_arch = "x86_64") {
            bail!("Playwright browser builds require x86-64 Windows");
        }
        Ok(HostPlatform {
            browser_artifact: "win64".to_owned(),
            native_dependencies: native_dependencies::Capability::NotRequired,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .context("failed to determine the macOS kernel release")?;
        if !output.status.success() {
            bail!("uname -r failed while locating browser builds");
        }
        let kernel_major: u32 = String::from_utf8(output.stdout)?
            .trim()
            .split('.')
            .next()
            .context("macOS kernel release is empty")?
            .parse()
            .context("macOS kernel release has no numeric major version")?;
        let mac_major = if kernel_major < 18 {
            "10.13".to_owned()
        } else if kernel_major == 18 {
            "10.14".to_owned()
        } else if kernel_major == 19 {
            "10.15".to_owned()
        } else if kernel_major < 25 {
            (kernel_major - 9).to_string()
        } else {
            (kernel_major + 1).min(26).to_string()
        };
        let apple_silicon = cfg!(target_arch = "aarch64");
        Ok(HostPlatform {
            browser_artifact: format!(
                "mac{mac_major}{}",
                if apple_silicon { "-arm64" } else { "" }
            ),
            native_dependencies: native_dependencies::Capability::NotRequired,
        })
    }
    #[cfg(target_os = "linux")]
    {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            bail!("Playwright browsers are unsupported on this Linux architecture");
        };
        let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
        Ok(linux_host_platform(&release, arch))
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        bail!("Playwright browser builds are unsupported on this Unix platform")
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_host_platform(release: &str, arch: &str) -> HostPlatform {
    use std::collections::HashMap;

    let fields = release
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            (
                key,
                value.trim_matches(|character| character == '"' || character == '\''),
            )
        })
        .collect::<HashMap<_, _>>();
    let id = fields.get("ID").copied().unwrap_or("");
    let version = fields.get("VERSION_ID").copied().unwrap_or("");
    let major = version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(24);
    let distribution = match id {
        "ubuntu" | "pop" | "neon" | "tuxedo" => Some(match major {
            0..=21 => "ubuntu20.04".to_owned(),
            22..=23 => "ubuntu22.04".to_owned(),
            24..=25 => "ubuntu24.04".to_owned(),
            26..=27 => "ubuntu26.04".to_owned(),
            _ => format!("ubuntu{version}"),
        }),
        "linuxmint" => Some(match major {
            0..=20 => "ubuntu20.04".to_owned(),
            21 => "ubuntu22.04".to_owned(),
            _ => "ubuntu24.04".to_owned(),
        }),
        "debian" | "raspbian" if ["11", "12", "13", ""].contains(&version) => Some(format!(
            "debian{}",
            if version.is_empty() { "13" } else { version }
        )),
        _ => None,
    };
    let browser_distribution = distribution.as_deref().unwrap_or("ubuntu24.04").to_owned();
    let native_dependencies = if let Some(distribution) = distribution {
        native_dependencies::Capability::Apt {
            registry_platform: format!("{distribution}-{arch}"),
        }
    } else {
        let distribution = match (id, version) {
            ("", _) => "unknown Linux distribution".to_owned(),
            (_, "") => id.to_owned(),
            _ => format!("{id} {version}"),
        };
        native_dependencies::Capability::Unsupported { distribution }
    };
    HostPlatform {
        browser_artifact: format!("{browser_distribution}-{arch}"),
        native_dependencies,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_registry_matches_playwright_directory_names() {
        let installation = PlaywrightInstallation {
            distribution: Registry::embedded().unwrap(),
            host: HostPlatform {
                browser_artifact: "win64".to_owned(),
                native_dependencies: native_dependencies::Capability::NotRequired,
            },
            browser_cache: PathBuf::from("browsers"),
        };
        let chromium = installation
            .artifact(BrowserName::ChromiumHeadlessShell)
            .unwrap();
        assert_eq!(
            chromium.directory,
            Path::new("browsers").join("chromium_headless_shell-1228")
        );
        assert_eq!(
            chromium.urls,
            [
                "https://cdn.playwright.dev/builds/cft/149.0.7827.55/win64/chrome-headless-shell-win64.zip"
            ]
        );
    }

    #[test]
    fn frozen_webkit_revision_uses_platform_specific_directory() {
        let installation = PlaywrightInstallation {
            distribution: Registry::embedded().unwrap(),
            host: HostPlatform {
                browser_artifact: "ubuntu20.04-x64".to_owned(),
                native_dependencies: native_dependencies::Capability::NotRequired,
            },
            browser_cache: PathBuf::from("browsers"),
        };
        let webkit = installation.artifact(BrowserName::Webkit).unwrap();
        assert_eq!(webkit.revision, "2092");
        assert_eq!(
            webkit.directory,
            Path::new("browsers").join("webkit_ubuntu20.04_x64_special-2092")
        );
        assert!(webkit.urls[0].ends_with("/builds/webkit/2092/webkit-ubuntu-20.04.zip"));
    }

    #[test]
    fn every_browser_download_source_requires_https() {
        let insecure_defaults = vec!["http://cdn.example.test".to_owned()];
        assert!(
            download_urls_from(
                "browser.zip",
                &insecure_defaults,
                None,
                "PLAYWRIGHT_FIREFOX_DOWNLOAD_HOST"
            )
            .unwrap_err()
            .to_string()
            .contains("absolute HTTPS URL")
        );

        let secure_defaults = vec!["https://cdn.example.test/root".to_owned()];
        assert!(
            download_urls_from(
                "browser.zip",
                &secure_defaults,
                Some("http://override.example.test"),
                "PLAYWRIGHT_FIREFOX_DOWNLOAD_HOST"
            )
            .unwrap_err()
            .to_string()
            .contains("absolute HTTPS URL")
        );
        assert_eq!(
            download_urls_from(
                "browser.zip",
                &secure_defaults,
                Some("https://override.example.test/root/"),
                "PLAYWRIGHT_FIREFOX_DOWNLOAD_HOST"
            )
            .unwrap(),
            ["https://override.example.test/root/browser.zip"]
        );
    }

    #[test]
    fn browser_requests_retry_only_transient_failures() {
        let mut transient_attempts = 0;
        let value = retry_request(|| {
            transient_attempts += 1;
            if transient_attempts == 1 {
                Err(ureq::Error::HostNotFound)
            } else {
                Ok(42)
            }
        })
        .unwrap();
        assert_eq!(value, 42);
        assert_eq!(transient_attempts, 2);

        let mut permanent_attempts = 0;
        let error = retry_request(|| {
            permanent_attempts += 1;
            Err::<(), _>(ureq::Error::StatusCode(404))
        })
        .unwrap_err();
        assert!(matches!(error, ureq::Error::StatusCode(404)));
        assert_eq!(permanent_attempts, 1);
    }

    #[test]
    fn unknown_linux_distribution_does_not_gain_apt_capability() {
        let host = linux_host_platform("ID=fedora\nVERSION_ID=43\n", "x64");
        assert_eq!(host.browser_artifact, "ubuntu24.04-x64");
        assert_eq!(
            host.native_dependencies,
            native_dependencies::Capability::Unsupported {
                distribution: "fedora 43".to_owned()
            }
        );
    }

    #[test]
    fn supported_linux_distribution_keeps_artifact_and_apt_decisions_distinct() {
        let host = linux_host_platform("ID=ubuntu\nVERSION_ID=\"24.04\"\n", "arm64");
        assert_eq!(host.browser_artifact, "ubuntu24.04-arm64");
        assert_eq!(
            host.native_dependencies,
            native_dependencies::Capability::Apt {
                registry_platform: "ubuntu24.04-arm64".to_owned()
            }
        );
    }

    #[test]
    fn symlink_targets_cannot_escape_the_archive() {
        assert!(validate_relative_target(Path::new("one/two"), Path::new("../target")).is_ok());
        assert!(validate_relative_target(Path::new("one"), Path::new("../../target")).is_err());
        assert!(validate_relative_target(Path::new("one"), Path::new("/target")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn browser_archives_cannot_alias_installation_bookkeeping_paths() {
        let directory = tempfile::tempdir().unwrap();
        for reserved in [INSTALLATION_MARKER, INSTALLATION_METADATA] {
            let archive_path = directory.path().join(format!("{reserved}.zip"));
            let mut archive = zip::ZipWriter::new(File::create(&archive_path).unwrap());
            archive
                .start_file("browser", zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(b"executable").unwrap();
            archive
                .add_symlink("alias", ".", zip::write::SimpleFileOptions::default())
                .unwrap();
            archive
                .add_symlink(
                    format!("alias/{reserved}"),
                    "browser",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            archive.finish().unwrap();
            let destination = directory.path().join(format!("{reserved}.out"));
            fs::create_dir(&destination).unwrap();

            extract_zip(&archive_path, &destination).unwrap();

            assert!(write_installation_receipt(&destination, "receipt").is_err());

            assert_eq!(
                fs::read(destination.join("browser")).unwrap(),
                b"executable"
            );
        }
    }
}
