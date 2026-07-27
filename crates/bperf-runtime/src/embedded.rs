use std::{
    env, fs,
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use tempfile::tempdir_in;

const ARCHIVE_MAGIC: &[u8] = b"BPERF_RUNTIME_1\n";
const ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/runtime.gz"));
const NPM_ENV: &str = "BPERF_NPM";

pub(crate) fn materialize(executable_directory: &Path) -> Result<PathBuf> {
    let parent = executable_directory
        .join("bperf-runtime")
        .join(env!("CARGO_PKG_VERSION"));
    let target = parent.join("sidecar");
    if target.exists() {
        return Ok(target);
    }

    fs::create_dir_all(&parent).with_context(|| {
        format!(
            "failed to create embedded runtime directory {}",
            parent.display()
        )
    })?;
    let temporary =
        tempdir_in(&parent).context("failed to create embedded runtime staging directory")?;
    let staged = temporary.path().join("sidecar");
    unpack(ARCHIVE, &staged)?;
    if !staged
        .join("node_modules")
        .join("esbuild")
        .join("package.json")
        .is_file()
    {
        install_dependencies(&staged)?;
    }

    match fs::rename(&staged, &target) {
        Ok(()) => Ok(target),
        Err(_) if target.exists() => Ok(target),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to activate embedded runtime at {}",
                target.display()
            )
        }),
    }
}

fn install_dependencies(root: &Path) -> Result<()> {
    let npm = env::var_os(NPM_ENV).map_or_else(
        || {
            if cfg!(windows) {
                PathBuf::from("npm.cmd")
            } else {
                PathBuf::from("npm")
            }
        },
        PathBuf::from,
    );
    let status = Command::new(&npm)
        .args(["ci", "--omit=dev", "--no-audit", "--no-fund"])
        .current_dir(root)
        .env("PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD", "1")
        .status()
        .with_context(|| {
            format!(
                "the Cargo-installed bperf binary needs its pinned benchmark runtime; failed to start {} (set {NPM_ENV} to override it)",
                npm.display()
            )
        })?;
    if !status.success() {
        bail!(
            "{} exited with {status} while installing the pinned bperf benchmark runtime",
            npm.display()
        );
    }
    Ok(())
}

fn unpack(bytes: &[u8], destination: &Path) -> Result<()> {
    let mut archive = GzDecoder::new(Cursor::new(bytes));
    let mut magic = vec![0; ARCHIVE_MAGIC.len()];
    archive
        .read_exact(&mut magic)
        .context("embedded runtime archive is truncated")?;
    if magic != ARCHIVE_MAGIC {
        bail!("embedded runtime archive has an unsupported format");
    }

    let files = read_u32(&mut archive)? as usize;
    for _ in 0..files {
        let path_length = read_u32(&mut archive)? as usize;
        if path_length == 0 || path_length > 16 * 1024 {
            bail!("embedded runtime archive contains an invalid path length");
        }
        let mut path = vec![0; path_length];
        archive.read_exact(&mut path)?;
        let path = PathBuf::from(
            String::from_utf8(path).context("embedded runtime archive path is not UTF-8")?,
        );
        if !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            bail!("embedded runtime archive contains an unsafe path");
        }
        let mode = read_u32(&mut archive)?;
        let body_length = read_u64(&mut archive)?;
        let target = destination.join(path);
        fs::create_dir_all(
            target
                .parent()
                .context("embedded runtime archive path has no parent")?,
        )?;
        let mut output = fs::File::create(&target)?;
        std::io::copy(&mut archive.by_ref().take(body_length), &mut output)?;
        if output.metadata()?.len() != body_length {
            bail!("embedded runtime archive is truncated");
        }
        set_mode(&target, mode)?;
    }
    let mut trailing = [0];
    if archive.read(&mut trailing)? != 0 {
        bail!("embedded runtime archive contains trailing data");
    }
    Ok(())
}

fn read_u32(input: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(input: &mut impl Read) -> Result<u64> {
    let mut bytes = [0; 8];
    input.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to set permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_runtime_contains_the_benchmark_host_contract() {
        let directory = tempfile::tempdir().unwrap();
        unpack(ARCHIVE, directory.path()).unwrap();

        for relative in [
            "package.json",
            "package-lock.json",
            "src/benchmark-host.ts",
            "src/browser-benchmark.ts",
            "src/project-modules.ts",
        ] {
            assert!(directory.path().join(relative).is_file(), "{relative}");
        }
    }
}
