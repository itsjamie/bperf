use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use flate2::{Compression, write::GzEncoder};

const ARCHIVE_MAGIC: &[u8] = b"BPERF_RUNTIME_1\n";
const RUNTIME_FILES: [&str; 5] = [
    "package.json",
    "package-lock.json",
    "src/benchmark-host.ts",
    "src/browser-benchmark.ts",
    "src/project-modules.ts",
];
const PRODUCTION_MODULES: [&str; 4] = [
    "node_modules/@esbuild",
    "node_modules/esbuild",
    "node_modules/playwright",
    "node_modules/playwright-core",
];

fn main() {
    if let Err(error) = build_archive() {
        panic!("failed to embed the bperf benchmark runtime: {error}");
    }
}

fn build_archive() -> io::Result<()> {
    println!("cargo:rerun-if-env-changed=BPERF_EMBEDDED_SIDECAR_DIR");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let development = manifest.join("../..").join("sidecar");
    for relative in RUNTIME_FILES {
        println!(
            "cargo:rerun-if-changed={}",
            development.join(relative).display()
        );
    }

    let configured = env::var_os("BPERF_EMBEDDED_SIDECAR_DIR");
    let root = configured
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or(development);
    let include_dependencies = configured.is_some();
    let mut files = Vec::new();
    for relative in RUNTIME_FILES {
        let path = root.join(relative);
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing runtime file {}", path.display()),
            ));
        }
        files.push((PathBuf::from(relative), path));
    }
    if include_dependencies {
        for relative in PRODUCTION_MODULES {
            let path = root.join(relative);
            if !path.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing production dependency {}", path.display()),
                ));
            }
            collect_files(&root, &path, &mut files)?;
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("runtime.gz");
    let file = fs::File::create(output)?;
    let mut archive = GzEncoder::new(file, Compression::best());
    archive.write_all(ARCHIVE_MAGIC)?;
    write_u32(&mut archive, files.len())?;
    for (relative, source) in files {
        let relative = relative.to_string_lossy().replace('\\', "/");
        let body = fs::read(&source)?;
        write_u32(&mut archive, relative.len())?;
        archive.write_all(relative.as_bytes())?;
        write_u32(&mut archive, file_mode(&source) as usize)?;
        write_u64(&mut archive, body.len() as u64)?;
        archive.write_all(&body)?;
    }
    archive.finish()?;
    Ok(())
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> io::Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_files(root, &path, files)?;
        } else if file_type.is_file() {
            let relative = path.strip_prefix(root).map_err(io::Error::other)?;
            files.push((relative.to_owned(), path));
        }
    }
    Ok(())
}

fn write_u32(output: &mut impl Write, value: usize) -> io::Result<()> {
    let value = u32::try_from(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "archive value exceeds u32"))?;
    output.write_all(&value.to_le_bytes())
}

fn write_u64(output: &mut impl Write, value: u64) -> io::Result<()> {
    output.write_all(&value.to_le_bytes())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(0o644)
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> u32 {
    0o644
}
