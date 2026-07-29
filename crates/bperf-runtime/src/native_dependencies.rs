//! Linux packages required by the generated Playwright browser registry.

use std::collections::BTreeSet;

#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::bail;

#[cfg(target_os = "linux")]
use crate::registry::DependencyGroups;
use crate::{installation::BrowserName, registry::Registry};

pub(crate) fn install(
    host: &str,
    browsers: &BTreeSet<BrowserName>,
    registry: &Registry,
) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (host, browsers, registry);
        println!("This platform does not require a separate system dependency installation.");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        let groups = registry.dependency_groups(host)?;
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
        run_apt(&["update"])?;
        let mut arguments = vec!["install", "-y", "--no-install-recommends"];
        arguments.extend(packages);
        run_apt(&arguments)
    }
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
}
