//! Host package-manager capability and browser-native dependency installation.

use std::collections::BTreeSet;

#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

#[cfg(any(target_os = "linux", test))]
use anyhow::Context;
use anyhow::{Result, bail};

#[cfg(target_os = "linux")]
use crate::registry::DependencyGroups;
use crate::{installation::BrowserName, registry::Registry};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Capability {
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    NotRequired,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Apt { registry_platform: String },
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Unsupported { distribution: String },
}

pub(crate) fn install(
    capability: &Capability,
    browsers: &BTreeSet<BrowserName>,
    registry: &Registry,
) -> Result<()> {
    match capability {
        Capability::NotRequired => {
            println!("This platform does not require a separate system dependency installation.");
            Ok(())
        }
        Capability::Apt { registry_platform } => install_apt(registry_platform, browsers, registry),
        Capability::Unsupported { distribution } => bail!(
            "automatic browser dependency installation is unsupported on {distribution}; install the missing shared libraries with the host package manager"
        ),
    }
}

#[cfg(target_os = "linux")]
fn install_apt(
    registry_platform: &str,
    browsers: &BTreeSet<BrowserName>,
    registry: &Registry,
) -> Result<()> {
    let groups = registry.dependency_groups(registry_platform)?;
    let mut packages = groups
        .tools
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for browser in browsers {
        let group = dependency_group(groups, *browser);
        packages.extend(group.iter().map(String::as_str));
    }
    let packages = packages.into_iter().collect::<Vec<_>>();
    println!(
        "Installing {} operating-system packages for the selected browsers...",
        packages.len()
    );
    let update_error = run_apt(&["update"]).err();
    let mut arguments = vec!["install", "-y", "--no-install-recommends"];
    arguments.extend(packages);
    complete_apt_install(update_error, run_apt(&arguments))
}

#[cfg(any(target_os = "linux", test))]
fn complete_apt_install(
    update_error: Option<anyhow::Error>,
    install_result: Result<()>,
) -> Result<()> {
    match (update_error, install_result) {
        (None, result) => result,
        (Some(update_error), Ok(())) => {
            eprintln!(
                "warning: apt package index refresh failed, but the requested browser dependencies were installed from authenticated cached indexes: {update_error:#}"
            );
            Ok(())
        }
        (Some(update_error), Err(install_error)) => Err(install_error)
            .with_context(|| format!("apt package index refresh also failed: {update_error:#}")),
    }
}

#[cfg(not(target_os = "linux"))]
fn install_apt(
    registry_platform: &str,
    _browsers: &BTreeSet<BrowserName>,
    _registry: &Registry,
) -> Result<()> {
    bail!("apt dependency installation is unavailable on this host (requested {registry_platform})")
}

#[cfg(target_os = "linux")]
fn dependency_group(groups: &DependencyGroups, browser: BrowserName) -> &[String] {
    match browser {
        BrowserName::ChromiumHeadlessShell => &groups.chromium,
        BrowserName::Firefox => &groups.firefox,
        BrowserName::Webkit => &groups.webkit,
    }
}

#[cfg(target_os = "linux")]
fn run_apt(arguments: &[&str]) -> Result<()> {
    let is_root = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to determine whether browser dependencies need sudo")?
        .stdout
        == b"0\n";
    let status = if is_root {
        Command::new("apt-get")
            .args(arguments)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to start apt-get")?
    } else {
        Command::new("sudo")
            .arg("--")
            .arg("apt-get")
            .args(arguments)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to start sudo for apt-get")?
    };
    if !status.success() {
        bail!("apt-get {} exited with {status}", arguments.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn architecture_uses_the_same_distribution_packages() {
        let registry = Registry::embedded().unwrap();
        assert!(std::ptr::eq(
            registry.dependency_groups("ubuntu24.04-x64").unwrap(),
            registry.dependency_groups("ubuntu24.04-arm64").unwrap()
        ));
    }

    #[test]
    fn every_supported_linux_platform_has_all_browser_groups() {
        let registry = Registry::embedded().unwrap();
        for platform in [
            "ubuntu20.04-x64",
            "ubuntu22.04-x64",
            "ubuntu24.04-x64",
            "ubuntu26.04-x64",
            "debian11-x64",
            "debian12-x64",
            "debian13-x64",
        ] {
            let groups = registry.dependency_groups(platform).unwrap();
            assert!(!groups.tools.is_empty());
            assert!(!groups.chromium.is_empty());
            assert!(!groups.firefox.is_empty());
            assert!(!groups.webkit.is_empty());
        }
    }

    #[test]
    fn successful_install_accepts_an_unrelated_index_refresh_failure() {
        assert!(
            complete_apt_install(
                Some(anyhow::anyhow!("third-party mirror is synchronizing")),
                Ok(())
            )
            .is_ok()
        );
    }

    #[test]
    fn failed_install_preserves_the_index_refresh_failure() {
        let error = complete_apt_install(
            Some(anyhow::anyhow!("index refresh failed")),
            Err(anyhow::anyhow!("package install failed")),
        )
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("index refresh failed"));
        assert!(message.contains("package install failed"));
    }
}
