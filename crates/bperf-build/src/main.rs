//! Repository packaging and installation contract checks.

mod playwright_registry;

use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

use anyhow::{Context, Result, bail};
use flate2::{Compression, write::GzEncoder};
use serde_json::json;
use sha2::{Digest, Sha256};
use tar::Builder;

fn main() -> Result<()> {
    let mut arguments = env::args_os().skip(1);
    match arguments.next().as_deref().and_then(OsStr::to_str) {
        Some("package") => {
            let install = match arguments.next().as_deref().and_then(OsStr::to_str) {
                None => false,
                Some("--install") => true,
                Some(argument) => bail!("unknown package argument {argument}"),
            };
            if arguments.next().is_some() {
                bail!("usage: bperf-build package [--install]");
            }
            package(install)
        }
        Some("verify-install") => {
            let kind = match arguments.next().as_deref().and_then(OsStr::to_str) {
                Some("release") => InstallationKind::Release,
                Some("source") => InstallationKind::Source,
                _ => bail!("usage: bperf-build verify-install <release|source>"),
            };
            if arguments.next().is_some() {
                bail!("usage: bperf-build verify-install <release|source>");
            }
            verify_install(kind)
        }
        Some("playwright-registry") => playwright_registry::command(arguments),
        _ => bail!(
            "usage: bperf-build <package [--install]|verify-install <release|source>|playwright-registry <update VERSION|check>>"
        ),
    }
}

fn package(install: bool) -> Result<()> {
    let repository = repository_root()?;
    let version = package_version(&repository)?;
    let playwright_version = playwright_registry::current_version()?;
    if env::var("GITHUB_REF_TYPE").as_deref() == Ok("tag") {
        let tag = env::var("GITHUB_REF_NAME").context("tag build has no GITHUB_REF_NAME")?;
        if tag != format!("v{version}") {
            bail!("release tag {tag} does not match Cargo version v{version}");
        }
    }

    let host_target = rust_host_target(&repository)?;
    let target = env::var("BPERF_RELEASE_TARGET").unwrap_or_else(|_| host_target.clone());
    if target != host_target {
        bail!("release target {target} does not match this native runner ({host_target})");
    }
    let executable_name = if cfg!(windows) { "bperf.exe" } else { "bperf" };
    let bundle_name = format!("bperf-{version}-{target}");
    let distribution_root = repository.join("dist");
    let bundle = distribution_root.join(&bundle_name);
    let archive_name = format!("{bundle_name}.tar.gz");
    let archive = distribution_root.join(&archive_name);
    ensure_child(&repository, &distribution_root)?;
    ensure_child(&distribution_root, &bundle)?;
    ensure_child(&distribution_root, &archive)?;

    run(
        Command::new(cargo_executable())
            .args(["build", "--release", "--locked", "--target", &target])
            .current_dir(&repository),
        "build the release binary",
    )?;

    remove_path(&distribution_root, &bundle)?;
    remove_path(&distribution_root, &archive)?;
    fs::create_dir_all(&bundle)?;
    let built_executable = repository
        .join("target")
        .join(&target)
        .join("release")
        .join(executable_name);
    let packaged_executable = bundle.join(executable_name);
    fs::copy(&built_executable, &packaged_executable).with_context(|| {
        format!(
            "failed to copy release executable {}",
            built_executable.display()
        )
    })?;
    for name in ["README.md", "CONTRIBUTING.md", "LICENSE"] {
        fs::copy(repository.join(name), bundle.join(name))
            .with_context(|| format!("failed to package {name}"))?;
    }
    for name in ["docs", "examples"] {
        copy_tree(&repository.join(name), &bundle.join(name))?;
    }
    copy_tree(
        &repository.join("skills").join("bperf-agent-loop"),
        &bundle.join("skills").join("bperf-agent-loop"),
    )?;

    let executable_sha256 = sha256_file(&packaged_executable)?;
    let build = json!({
        "schema_version": 3,
        "name": "bperf",
        "version": version,
        "platform": env::consts::OS,
        "architecture": env::consts::ARCH,
        "target": target,
        "browser_distribution": {
            "provider": "playwright",
            "provider_version": playwright_version,
            "installer": "rust"
        },
        "browser_adapters": {
            "chromium": "rust-chromium",
            "firefox": "rust-firefox",
            "webkit": "rust-webkit"
        },
        "protocols": {
            "capture": 13,
            "benchmark_host": 2,
            "environment_schema": 6,
            "doctor_schema": 2
        },
        "executable_sha256": executable_sha256
    });
    fs::write(
        bundle.join("BUILD.json"),
        format!("{}\n", serde_json::to_string_pretty(&build)?),
    )?;

    if install {
        install_bundle(&bundle, &version, executable_name)?;
    }

    let output = File::create(&archive)?;
    let encoder = GzEncoder::new(output, Compression::best());
    let mut tar = Builder::new(encoder);
    tar.append_dir_all(&bundle_name, &bundle)?;
    let encoder = tar.into_inner()?;
    encoder.finish()?;

    let archive_sha256 = sha256_file(&archive)?;
    fs::write(
        PathBuf::from(format!("{}.sha256", archive.display())),
        format!("{archive_sha256}  {archive_name}\n"),
    )?;
    println!("{}", archive.display());
    Ok(())
}

fn install_bundle(bundle: &Path, version: &str, executable_name: &str) -> Result<()> {
    let install_root = env::var_os("BPERF_INSTALL_ROOT")
        .or_else(|| env::var_os("CARGO_HOME"))
        .map(PathBuf::from)
        .unwrap_or(home_directory()?.join(".cargo"));
    let binary_directory = install_root.join("bin");
    fs::create_dir_all(&binary_directory)?;
    let executable = binary_directory.join(executable_name);
    fs::copy(bundle.join(executable_name), &executable)?;
    set_executable(&executable)?;
    fs::write(
        binary_directory.join(format!("bperf-{version}.installed")),
        b"rust-native-browser-runtime\n",
    )?;
    Ok(())
}

#[derive(Clone, Copy)]
enum InstallationKind {
    Release,
    Source,
}

fn verify_install(kind: InstallationKind) -> Result<()> {
    let repository = repository_root()?;
    let cargo_home = required_directory("BPERF_TEST_CARGO_HOME")?;
    let scratch = required_directory("BPERF_TEST_ROOT")?;
    let executable = cargo_home
        .join("bin")
        .join(if cfg!(windows) { "bperf.exe" } else { "bperf" });
    if !executable.is_file() {
        bail!(
            "installed bperf executable is missing at {}",
            executable.display()
        );
    }
    if contains_node_runtime(&cargo_home)? {
        bail!(
            "installation unexpectedly contains a Node runtime beneath {}",
            cargo_home.display()
        );
    }

    run_bperf(&executable, &repository, ["--version"])?;
    let is_release = matches!(kind, InstallationKind::Release);
    let mut install_arguments = vec!["browsers", "install", "--engine"];
    install_arguments.push(if is_release { "all" } else { "chromium" });
    if cfg!(target_os = "linux") {
        install_arguments.push("--with-deps");
    }
    run_bperf(&executable, &repository, install_arguments)?;

    run_bperf(
        &executable,
        &repository,
        [
            "doctor",
            "--engine",
            if is_release { "all" } else { "chromium" },
            "--artifact-dir",
            path_argument(&scratch.join("doctor"))?,
        ],
    )?;

    if is_release {
        run_bperf(
            &executable,
            &repository,
            [
                "run",
                path_argument(
                    &repository
                        .join("examples")
                        .join("managed")
                        .join("fragment-parser.bench.ts"),
                )?,
                "--budget",
                "30s",
                "--message",
                "Verify installed release package",
                "--artifact-dir",
                path_argument(&scratch.join("measurements"))?,
                "--state-dir",
                path_argument(&scratch.join("managed"))?,
                "--object-dir",
                path_argument(&scratch.join("objects"))?,
                "--registry-dir",
                path_argument(&scratch.join("baselines"))?,
                "--comparison-dir",
                path_argument(&scratch.join("comparisons"))?,
                "--lineage-dir",
                path_argument(&scratch.join("lineages"))?,
            ],
        )?;
    }
    Ok(())
}

fn run_bperf<I, S>(executable: &Path, repository: &Path, arguments: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(executable);
    command.args(arguments).current_dir(repository);
    run(&mut command, "run the installed bperf contract")
}

fn run(command: &mut Command, action: &str) -> Result<()> {
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to {action}"))?;
    require_success(status, action)
}

fn require_success(status: ExitStatus, action: &str) -> Result<()> {
    if !status.success() {
        bail!("failed to {action}: process exited with {status}");
    }
    Ok(())
}

fn repository_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .context("failed to resolve the repository root")
}

fn package_version(repository: &Path) -> Result<String> {
    let manifest = fs::read_to_string(repository.join("Cargo.toml"))?;
    manifest
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
                .map(str::to_owned)
        })
        .context("Cargo.toml has no package version")
}

fn rust_host_target(repository: &Path) -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .current_dir(repository)
        .output()
        .context("failed to run rustc -vV")?;
    require_success(output.status, "query the Rust host target")?;
    String::from_utf8(output.stdout)?
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .context("rustc -vV did not report a host target")
}

fn cargo_executable() -> &'static str {
    if cfg!(windows) { "cargo.exe" } else { "cargo" }
}

fn required_directory(name: &str) -> Result<PathBuf> {
    let value = env::var_os(name).with_context(|| format!("{name} is required"))?;
    let path = PathBuf::from(value);
    fs::create_dir_all(&path)?;
    path.canonicalize()
        .with_context(|| format!("failed to resolve {name}"))
}

fn contains_node_runtime(root: &Path) -> Result<bool> {
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                if entry.file_name() == "node_modules" {
                    return Ok(true);
                }
                pending.push(entry.path());
            }
        }
    }
    Ok(false)
}

fn path_argument(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if matches!(entry.file_name().to_str(), Some("target" | "node_modules")) {
                continue;
            }
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        } else {
            bail!(
                "release source contains an unsupported filesystem entry {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut digest = Sha256::new();
    io::copy(&mut input, &mut DigestWriter(&mut digest))?;
    Ok(format!("{:x}", digest.finalize()))
}

struct DigestWriter<'a>(&'a mut Sha256);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn remove_path(root: &Path, target: &Path) -> Result<()> {
    ensure_child(root, target)?;
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(target)?,
        Ok(_) => fs::remove_file(target)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn ensure_child(root: &Path, target: &Path) -> Result<()> {
    let root = absolute_lexical(root)?;
    let target = absolute_lexical(target)?;
    let relative = target.strip_prefix(&root).ok();
    if relative.is_none_or(|relative| {
        relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
    }) {
        bail!(
            "refusing path outside {}: {}",
            root.display(),
            target.display()
        );
    }
    Ok(())
}

fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn home_directory() -> Result<PathBuf> {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .context("cannot locate the home directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_paths_must_remain_beneath_the_distribution_root() {
        let root = Path::new("repository").join("dist");
        assert!(ensure_child(&root, &root.join("bundle")).is_ok());
        assert!(ensure_child(&root, &root).is_err());
        assert!(ensure_child(&root, &root.join("..").join("outside")).is_err());
    }
}
