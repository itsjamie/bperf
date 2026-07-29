//! Pinned browser distribution discovery and installation.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::playwright::PlaywrightInstallation;

#[derive(Clone, Debug)]
pub struct BrowserInstallation {
    playwright: PlaywrightInstallation,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
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

    #[cfg(target_os = "linux")]
    pub(crate) const fn dependency_group(self) -> &'static str {
        match self {
            Self::ChromiumHeadlessShell => "chromium",
            Self::Firefox => "firefox",
            Self::Webkit => "webkit",
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstalledBrowser {
    pub(crate) browser_version: String,
    pub(crate) directory: PathBuf,
    pub(crate) executable: PathBuf,
    pub(crate) revision: String,
}

impl InstalledBrowser {
    pub fn browser_version(&self) -> &str {
        &self.browser_version
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
}

impl BrowserInstallation {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            playwright: PlaywrightInstallation::discover()?,
        })
    }

    pub fn browser(&self, name: BrowserName) -> Result<InstalledBrowser> {
        self.playwright.browser(name)
    }

    pub fn playwright_version(&self) -> &str {
        self.playwright.version()
    }

    pub fn install_browsers(
        &self,
        browsers: &[BrowserName],
        with_dependencies: bool,
    ) -> Result<()> {
        self.playwright.install(browsers, with_dependencies)
    }
}

/// Produces a portable absolute path string. Windows verbatim paths are valid
/// for filesystem APIs but are not stable identities and confuse some tooling.
pub fn portable_path(path: &Path) -> String {
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
