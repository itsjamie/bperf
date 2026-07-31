//! Validated access to the generated Playwright distribution registry.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const REGISTRY_SCHEMA_VERSION: u32 = 1;
const EMBEDDED_REGISTRY: &str = include_str!("../playwright-registry.json");

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Registry {
    schema_version: u32,
    source: RegistrySource,
    platforms: BTreeMap<String, BTreeMap<String, RegistryArtifact>>,
    linux_dependencies: BTreeMap<String, DependencyGroups>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrySource {
    package: String,
    version: String,
    integrity: String,
    signature: RegistrySignature,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrySignature {
    #[serde(rename = "keyid")]
    key_id: String,
    sig: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryArtifact {
    pub(crate) browser_version: String,
    pub(crate) revision: String,
    pub(crate) directory: String,
    pub(crate) executable: Vec<String>,
    pub(crate) download_path: String,
    pub(crate) mirrors: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DependencyGroups {
    pub(crate) tools: Vec<String>,
    pub(crate) chromium: Vec<String>,
    pub(crate) firefox: Vec<String>,
    pub(crate) webkit: Vec<String>,
}

impl Registry {
    pub(crate) fn embedded() -> Result<Self> {
        let registry: Self = serde_json::from_str(EMBEDDED_REGISTRY)
            .context("embedded Playwright registry is invalid JSON")?;
        registry.validate()?;
        Ok(registry)
    }

    pub(crate) fn version(&self) -> &str {
        &self.source.version
    }

    pub(crate) fn artifact(&self, host: &str, browser: &str) -> Result<&RegistryArtifact> {
        self.platforms
            .get(host)
            .and_then(|artifacts| artifacts.get(browser))
            .with_context(|| {
                format!(
                    "Playwright {} has no {browser} browser archive for {host}",
                    self.version()
                )
            })
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn dependency_groups(&self, host: &str) -> Result<&DependencyGroups> {
        let platform = host
            .strip_suffix("-arm64")
            .or_else(|| host.strip_suffix("-x64"))
            .unwrap_or(host);
        self.linux_dependencies.get(platform).with_context(|| {
            format!(
                "automatic browser dependency installation is unsupported on {host}; install the missing shared libraries with the host package manager"
            )
        })
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != REGISTRY_SCHEMA_VERSION {
            bail!(
                "embedded Playwright registry schema {} is unsupported",
                self.schema_version
            );
        }
        if self.source.package != "playwright-core"
            || self.source.version.trim().is_empty()
            || !self.source.integrity.starts_with("sha512-")
            || self.source.signature.key_id.trim().is_empty()
            || self.source.signature.sig.trim().is_empty()
        {
            bail!("embedded Playwright registry has invalid source authentication metadata");
        }
        if self.platforms.is_empty() {
            bail!("embedded Playwright registry has no platforms");
        }
        for (host, artifacts) in &self.platforms {
            if host.trim().is_empty() || artifacts.is_empty() {
                bail!("embedded Playwright registry contains an empty platform");
            }
            for (browser, artifact) in artifacts {
                validate_artifact(host, browser, artifact)?;
            }
        }
        for (platform, groups) in &self.linux_dependencies {
            if platform.trim().is_empty()
                || groups.tools.is_empty()
                || groups.chromium.is_empty()
                || groups.firefox.is_empty()
                || groups.webkit.is_empty()
            {
                bail!("embedded Playwright dependency groups are incomplete for {platform}");
            }
        }
        Ok(())
    }
}

fn validate_artifact(host: &str, browser: &str, artifact: &RegistryArtifact) -> Result<()> {
    if browser.trim().is_empty()
        || artifact.browser_version.trim().is_empty()
        || artifact.revision.trim().is_empty()
        || !is_normal_component(&artifact.directory)
        || artifact.executable.is_empty()
        || artifact
            .executable
            .iter()
            .any(|component| !is_normal_component(component))
        || !is_relative_download_path(&artifact.download_path)
        || artifact.mirrors.is_empty()
        || artifact
            .mirrors
            .iter()
            .any(|mirror| !mirror.starts_with("https://"))
    {
        bail!("embedded Playwright artifact {browser} for {host} is invalid");
    }
    Ok(())
}

fn is_normal_component(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

fn is_relative_download_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_registry_contains_every_current_engine() {
        let registry = Registry::embedded().unwrap();
        for host in [
            "win64",
            "ubuntu24.04-x64",
            "ubuntu24.04-arm64",
            "mac15",
            "mac15-arm64",
        ] {
            for browser in ["chromium-headless-shell", "firefox", "webkit"] {
                registry.artifact(host, browser).unwrap();
            }
        }
    }

    #[test]
    fn generated_registry_preserves_platform_overrides() {
        let registry = Registry::embedded().unwrap();
        let webkit = registry.artifact("ubuntu20.04-x64", "webkit").unwrap();
        assert_eq!(webkit.revision, "2092");
        assert_eq!(webkit.directory, "webkit_ubuntu20.04_x64_special-2092");
    }
}
