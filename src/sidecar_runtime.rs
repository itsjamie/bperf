//! Discovery and validation of the installed Node sidecar.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const SIDECAR_DIRECTORY_ENV: &str = "BPERF_SIDECAR_DIR";
const CAPTURE_ENTRYPOINT: &str = "bperf-sidecar.ts";
const BENCHMARK_HOST: &str = "benchmark-host.ts";

#[derive(Clone, Debug)]
pub(crate) struct SidecarInstallation {
    root: PathBuf,
}

impl SidecarInstallation {
    pub(crate) fn discover() -> Result<Self> {
        discover_from(
            std::env::var_os(SIDECAR_DIRECTORY_ENV),
            std::env::current_exe().ok().as_deref(),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("sidecar"),
        )
    }

    pub(crate) fn capture_entrypoint(&self) -> PathBuf {
        self.root.join("src").join(CAPTURE_ENTRYPOINT)
    }

    pub(crate) fn benchmark_host(&self) -> PathBuf {
        self.root.join("src").join(BENCHMARK_HOST)
    }

    pub(crate) fn identity_files(&self) -> Result<BTreeSet<PathBuf>> {
        fn collect(directory: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
            for entry in fs::read_dir(directory)
                .with_context(|| format!("failed to read {}", directory.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, files)?;
                } else if path.is_file() {
                    files.insert(path);
                }
            }
            Ok(())
        }

        let mut files = BTreeSet::new();
        collect(&self.root.join("src"), &mut files)?;
        for name in ["package.json", "package-lock.json"] {
            files.insert(self.root.join(name));
        }
        Ok(files)
    }
}

/// Preserves absolute path identity without Windows verbatim syntax, which Node
/// interprets incorrectly when the path names its entry module.
pub(crate) fn node_path(path: &Path) -> String {
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
    development: PathBuf,
) -> Result<SidecarInstallation> {
    if let Some(configured) = configured {
        return validate(PathBuf::from(configured)).with_context(|| {
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
                return validate(candidate);
            }
        }
    }

    if development.exists() {
        return validate(development);
    }

    bail!(
        "could not find the bperf Node sidecar; install the release bundle beside the executable or set {SIDECAR_DIRECTORY_ENV}"
    )
}

fn validate(root: PathBuf) -> Result<SidecarInstallation> {
    let root = fs::canonicalize(&root)
        .with_context(|| format!("failed to resolve sidecar directory {}", root.display()))?;
    for relative in [
        Path::new("src").join(CAPTURE_ENTRYPOINT),
        Path::new("src").join(BENCHMARK_HOST),
        PathBuf::from("package.json"),
        PathBuf::from("package-lock.json"),
        Path::new("node_modules")
            .join("playwright")
            .join("package.json"),
        Path::new("node_modules")
            .join("esbuild")
            .join("package.json"),
    ] {
        let required = root.join(relative);
        if !required.is_file() {
            bail!(
                "sidecar installation is incomplete; missing {}",
                required.display()
            );
        }
    }
    Ok(SidecarInstallation { root })
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

        let runtime = discover_from(None, Some(&executable), development).unwrap();

        assert_eq!(
            runtime.capture_entrypoint(),
            fs::canonicalize(installed)
                .unwrap()
                .join("src")
                .join(CAPTURE_ENTRYPOINT)
        );
    }

    #[test]
    fn configured_sidecar_fails_instead_of_falling_back() {
        let directory = tempdir().unwrap();
        let development = directory.path().join("development");
        fake_installation(&development);
        let missing = directory.path().join("missing");

        let error = discover_from(Some(missing.into_os_string()), None, development).unwrap_err();

        assert!(
            format!("{error:#}")
                .contains("BPERF_SIDECAR_DIR does not name a usable sidecar installation")
        );
    }

    fn fake_installation(root: &Path) {
        for relative in [
            Path::new("src").join(CAPTURE_ENTRYPOINT),
            Path::new("src").join(BENCHMARK_HOST),
            PathBuf::from("package.json"),
            PathBuf::from("package-lock.json"),
            Path::new("node_modules")
                .join("playwright")
                .join("package.json"),
            Path::new("node_modules")
                .join("esbuild")
                .join("package.json"),
        ] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}").unwrap();
        }
    }
}
