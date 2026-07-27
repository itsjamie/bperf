//! Discovery and validation of the installed bperf runtime.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::{embedded, playwright::PlaywrightInstallation};

const SIDECAR_DIRECTORY_ENV: &str = "BPERF_SIDECAR_DIR";
const BENCHMARK_HOST: &str = "benchmark-host.ts";
const RUNTIME_SOURCE_FILES: [&str; 3] =
    [BENCHMARK_HOST, "browser-benchmark.ts", "project-modules.ts"];

#[derive(Clone, Debug)]
pub struct RuntimeInstallation {
    playwright: PlaywrightInstallation,
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserName {
    ChromiumHeadlessShell,
    Firefox,
    Webkit,
}

impl BrowserName {
    pub(crate) const fn registry_name(self) -> &'static str {
        match self {
            Self::ChromiumHeadlessShell => "chromium-headless-shell",
            Self::Firefox => "firefox",
            Self::Webkit => "webkit",
        }
    }

    const fn install_name(self) -> &'static str {
        match self {
            Self::ChromiumHeadlessShell => "chromium",
            Self::Firefox => "firefox",
            Self::Webkit => "webkit",
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstalledBrowser {
    pub(crate) directory: PathBuf,
    pub(crate) revision: String,
    pub(crate) browser_version: String,
}

impl InstalledBrowser {
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn browser_version(&self) -> &str {
        &self.browser_version
    }
}

impl RuntimeInstallation {
    pub fn discover() -> Result<Self> {
        let configured = std::env::var_os(SIDECAR_DIRECTORY_ENV);
        let executable = std::env::current_exe().ok();
        let development = cfg!(debug_assertions).then(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join("sidecar")
        });
        if let Some(runtime) = discover_from(configured, executable.as_deref(), development)? {
            return Ok(runtime);
        }

        let executable_directory = executable
            .as_deref()
            .and_then(Path::parent)
            .context("could not locate the bperf executable to materialize its bundled runtime")?;
        let root = embedded::materialize(executable_directory)?;
        validate(root)
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self> {
        validate(root.into())
    }

    pub fn benchmark_host(&self) -> PathBuf {
        self.root.join("src").join(BENCHMARK_HOST)
    }

    pub fn identity_files(&self) -> BTreeSet<PathBuf> {
        let mut files = RUNTIME_SOURCE_FILES
            .iter()
            .map(|name| self.root.join("src").join(name))
            .collect::<BTreeSet<_>>();
        for name in ["package.json", "package-lock.json"] {
            files.insert(self.root.join(name));
        }
        files
    }

    pub fn browser(&self, name: BrowserName) -> Result<InstalledBrowser> {
        self.playwright.browser(name)
    }

    pub fn playwright_version(&self) -> &str {
        self.playwright.version()
    }

    pub fn install_browsers(
        &self,
        node: &Path,
        browsers: &[BrowserName],
        with_dependencies: bool,
    ) -> Result<()> {
        let cli = self
            .root
            .join("node_modules")
            .join("playwright")
            .join("cli.js");
        let mut command = Command::new(node);
        command.arg(node_path(&cli)).arg("install");
        if with_dependencies {
            command.arg("--with-deps");
        }
        command.args(browsers.iter().map(|browser| browser.install_name()));
        let status = command.status().with_context(|| {
            format!(
                "failed to start Playwright {} with Node executable {}",
                self.playwright.version(),
                node.display()
            )
        })?;
        if !status.success() {
            bail!(
                "Playwright {} browser installation exited with {status}",
                self.playwright.version()
            );
        }
        Ok(())
    }
}

/// Preserves absolute path identity without Windows verbatim syntax, which Node
/// interprets incorrectly when the path names its entry module.
pub fn node_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(path) = value.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{path}");
        }
        if let Some(path) = value.strip_prefix(r"\\?\") {
            return path.to_owned();
        }
    }
    value.into_owned()
}

fn discover_from(
    configured: Option<OsString>,
    executable: Option<&Path>,
    development: Option<PathBuf>,
) -> Result<Option<RuntimeInstallation>> {
    if let Some(configured) = configured {
        return validate(PathBuf::from(configured))
            .map(Some)
            .with_context(|| {
                format!("{SIDECAR_DIRECTORY_ENV} does not name a usable sidecar installation")
            });
    }

    if let Some(executable_directory) = executable.and_then(Path::parent) {
        for candidate in [
            executable_directory.join("sidecar"),
            executable_directory
                .join("bperf-runtime")
                .join(env!("CARGO_PKG_VERSION"))
                .join("sidecar"),
        ] {
            if candidate.exists() {
                return validate(candidate).map(Some);
            }
        }
    }

    if let Some(development) = development
        && development.exists()
    {
        return validate(development).map(Some);
    }

    Ok(None)
}

fn validate(root: PathBuf) -> Result<RuntimeInstallation> {
    let root = fs::canonicalize(&root)
        .with_context(|| format!("failed to resolve sidecar directory {}", root.display()))?;
    for relative in RUNTIME_SOURCE_FILES
        .iter()
        .map(|name| Path::new("src").join(name))
        .chain([
            PathBuf::from("package.json"),
            PathBuf::from("package-lock.json"),
            Path::new("node_modules")
                .join("playwright")
                .join("package.json"),
            Path::new("node_modules")
                .join("playwright-core")
                .join("browsers.json"),
            Path::new("node_modules")
                .join("playwright-core")
                .join("package.json"),
            Path::new("node_modules")
                .join("esbuild")
                .join("package.json"),
        ])
    {
        let required = root.join(relative);
        if !required.is_file() {
            bail!(
                "sidecar installation is incomplete; missing {}",
                required.display()
            );
        }
    }
    let playwright = PlaywrightInstallation::discover(&root)?;
    Ok(RuntimeInstallation { playwright, root })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn installed_sidecar_takes_precedence_over_the_development_checkout() {
        let directory = tempdir().unwrap();
        let installed = directory.path().join("installed").join("sidecar");
        let development = directory.path().join("development");
        fake_installation(&installed);
        fake_installation(&development);
        let executable =
            installed
                .parent()
                .unwrap()
                .join(if cfg!(windows) { "bperf.exe" } else { "bperf" });

        let runtime = discover_from(None, Some(&executable), Some(development))
            .unwrap()
            .unwrap();

        assert_eq!(
            runtime.benchmark_host(),
            fs::canonicalize(installed)
                .unwrap()
                .join("src")
                .join(BENCHMARK_HOST)
        );
    }

    #[test]
    fn configured_sidecar_fails_instead_of_falling_back() {
        let directory = tempdir().unwrap();
        let development = directory.path().join("development");
        fake_installation(&development);
        let missing = directory.path().join("missing");

        let error =
            discover_from(Some(missing.into_os_string()), None, Some(development)).unwrap_err();

        assert!(
            format!("{error:#}")
                .contains("BPERF_SIDECAR_DIR does not name a usable sidecar installation")
        );
    }

    #[test]
    fn no_external_runtime_defers_to_the_embedded_distribution() {
        let directory = tempdir().unwrap();

        assert!(
            discover_from(
                None,
                Some(&directory.path().join("bin").join("bperf")),
                Some(directory.path().join("development")),
            )
            .unwrap()
            .is_none()
        );
    }

    fn fake_installation(root: &Path) {
        for relative in RUNTIME_SOURCE_FILES
            .iter()
            .map(|name| Path::new("src").join(name))
            .chain([
                PathBuf::from("package.json"),
                PathBuf::from("package-lock.json"),
                Path::new("node_modules")
                    .join("playwright")
                    .join("package.json"),
                Path::new("node_modules")
                    .join("playwright-core")
                    .join("browsers.json"),
                Path::new("node_modules")
                    .join("playwright-core")
                    .join("package.json"),
                Path::new("node_modules")
                    .join("esbuild")
                    .join("package.json"),
            ])
        {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}").unwrap();
        }
        fs::write(
            root.join("node_modules/playwright-core/browsers.json"),
            br#"{"browsers":[]}"#,
        )
        .unwrap();
        fs::write(
            root.join("node_modules/playwright-core/package.json"),
            br#"{"version":"test"}"#,
        )
        .unwrap();
    }
}
