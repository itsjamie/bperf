//! Playwright browser discovery for an installed bperf runtime.
//!
//! Engine adapters ask for an installed browser by registry name. Cache location,
//! host-specific revision overrides, and Playwright's on-disk naming convention
//! remain private to this module.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::installation::{BrowserName, InstalledBrowser};

#[cfg(all(unix, not(target_os = "macos")))]
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub(crate) struct PlaywrightInstallation {
    browsers: Vec<BrowserDescriptor>,
    host_platform: String,
    registry: PathBuf,
    version: String,
}

impl PlaywrightInstallation {
    pub(crate) fn discover(sidecar_root: &Path) -> Result<Self> {
        let core = sidecar_root.join("node_modules").join("playwright-core");
        let browsers_path = core.join("browsers.json");
        let browsers: BrowsersJson = serde_json::from_slice(
            &fs::read(&browsers_path)
                .with_context(|| format!("failed to read {}", browsers_path.display()))?,
        )
        .with_context(|| {
            format!(
                "invalid Playwright browser registry {}",
                browsers_path.display()
            )
        })?;
        let package_path = core.join("package.json");
        let package: PackageJson = serde_json::from_slice(
            &fs::read(&package_path)
                .with_context(|| format!("failed to read {}", package_path.display()))?,
        )
        .with_context(|| format!("invalid Playwright package {}", package_path.display()))?;

        Ok(Self {
            browsers: browsers.browsers,
            host_platform: playwright_host_platform()?,
            registry: playwright_registry_directory(&core)?,
            version: package.version,
        })
    }

    pub(crate) fn browser(&self, name: BrowserName) -> Result<InstalledBrowser> {
        let name = name.registry_name();
        let descriptor = self
            .browsers
            .iter()
            .find(|browser| browser.name == name)
            .with_context(|| format!("pinned Playwright registry has no {name} descriptor"))?;
        let overridden_revision = descriptor.revision_overrides.get(&self.host_platform);
        let revision = overridden_revision
            .cloned()
            .unwrap_or_else(|| descriptor.revision.clone());
        let directory_prefix = if overridden_revision.is_some() {
            format!("{name}_{}_special", self.host_platform)
        } else {
            name.to_owned()
        };

        Ok(InstalledBrowser {
            directory: self
                .registry
                .join(playwright_browser_directory(&directory_prefix, &revision)),
            revision,
            browser_version: descriptor
                .browser_version
                .clone()
                .with_context(|| format!("Playwright {name} descriptor has no browser version"))?,
        })
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Deserialize)]
struct BrowsersJson {
    browsers: Vec<BrowserDescriptor>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserDescriptor {
    name: String,
    revision: String,
    #[serde(default)]
    revision_overrides: BTreeMap<String, String>,
    browser_version: Option<String>,
}

#[derive(Deserialize)]
struct PackageJson {
    version: String,
}

fn playwright_browser_directory(prefix: &str, revision: &str) -> String {
    format!("{}-{revision}", prefix.replace('-', "_"))
}

fn playwright_registry_directory(core: &Path) -> Result<PathBuf> {
    let configured = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH");
    let path = match configured.as_deref() {
        Some(value) if value == OsStr::new("0") => core.join(".local-browsers"),
        Some(value) if !value.is_empty() => {
            let configured = PathBuf::from(value);
            if configured.is_absolute() {
                configured
            } else {
                nonempty_env_path("INIT_CWD")
                    .unwrap_or(std::env::current_dir()?)
                    .join(configured)
            }
        }
        _ => {
            #[cfg(windows)]
            {
                nonempty_env_path("LOCALAPPDATA")
                    .or_else(|| {
                        nonempty_env_path("USERPROFILE")
                            .map(|home| home.join("AppData").join("Local"))
                    })
                    .context("cannot locate the Windows Playwright browser cache")?
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

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(unix)]
fn home_directory() -> Result<PathBuf> {
    nonempty_env_path("HOME")
        .context("cannot locate the Playwright browser cache because HOME is unset")
}

fn playwright_host_platform() -> Result<String> {
    if let Some(overridden) =
        std::env::var_os("PLAYWRIGHT_HOST_PLATFORM_OVERRIDE").filter(|value| !value.is_empty())
    {
        return overridden
            .into_string()
            .map_err(|_| anyhow::anyhow!("PLAYWRIGHT_HOST_PLATFORM_OVERRIDE is not UTF-8"));
    }
    #[cfg(windows)]
    {
        Ok("win64".to_owned())
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("uname")
            .arg("-r")
            .output()
            .context("failed to determine the macOS kernel release")?;
        if !output.status.success() {
            anyhow::bail!("uname -r failed while locating Playwright browsers");
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
        let apple_silicon = std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("Apple"));
        Ok(format!(
            "mac{mac_major}{}",
            if apple_silicon { "-arm64" } else { "" }
        ))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            anyhow::bail!("Playwright browsers are unsupported on this Linux architecture");
        };
        let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
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
            "ubuntu" | "pop" | "neon" | "tuxedo" => match major {
                0..=19 => "ubuntu18.04".to_owned(),
                20..=21 => "ubuntu20.04".to_owned(),
                22..=23 => "ubuntu22.04".to_owned(),
                24..=25 => "ubuntu24.04".to_owned(),
                26..=27 => "ubuntu26.04".to_owned(),
                _ => format!("ubuntu{version}"),
            },
            "linuxmint" => match major {
                0..=20 => "ubuntu20.04".to_owned(),
                21 => "ubuntu22.04".to_owned(),
                _ => "ubuntu24.04".to_owned(),
            },
            "debian" | "raspbian" if ["11", "12", "13", ""].contains(&version) => {
                format!("debian{}", if version.is_empty() { "13" } else { version })
            }
            _ => "ubuntu24.04".to_owned(),
        };
        Ok(format!("{distribution}-{arch}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_overrides_use_playwright_directory_names() {
        assert_eq!(
            playwright_browser_directory("webkit_mac14-arm64_special", "2251"),
            "webkit_mac14_arm64_special-2251"
        );

        let core = Path::new("sidecar")
            .join("node_modules")
            .join("playwright-core");
        if std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").is_none() {
            let path = playwright_registry_directory(&core).unwrap();
            assert!(path.ends_with("ms-playwright"));
        }
    }
}
