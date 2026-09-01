//! Browser-targeted project bundling and bundle identity.

use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use bperf_runtime::installation::portable_path;
use rolldown::{
    AttachDebugInfo, BundlerBuilder, BundlerOptions, BundlerTransformOptions, CodeSplittingMode,
    Either, ExperimentalOptions, InputItem, LegalComments, OutputFormat, Platform, ResolveOptions,
    SourceMapPathTransform, SourceMapType, TreeshakeOptions, TsConfig,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::runtime::Builder;

const ROLLDOWN_VERSION: &str = env!("BPERF_ROLLDOWN_VERSION");
const BROWSER_SDK_SOURCE: &str = include_str!("browser-benchmark.ts");
const BUNDLE_FILE: &str = "browser-bundle.js";
const METADATA_FILE: &str = "browser-bundle.json";
const RESOLUTION_FILES: [&str; 9] = [
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "tsconfig.json",
    "jsconfig.json",
];

/// A browser bundle and the complete identity needed to serve it later.
#[derive(Clone, Debug)]
pub(crate) struct BrowserProjectBundle {
    bundle_file: PathBuf,
    metadata_file: PathBuf,
    source_files: Vec<PathBuf>,
}

impl BrowserProjectBundle {
    pub(crate) fn open(
        root: &Path,
        entry: &Path,
        bundle_file: &Path,
        metadata_file: &Path,
    ) -> Result<Self> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve benchmark root {}", root.display()))?;
        let entry = project_file(&root, entry, "benchmark module")?;
        let bundle_file = canonical_file(bundle_file, "browser bundle")?;
        let metadata_file = canonical_file(metadata_file, "browser bundle metadata")?;
        let metadata: BundleMetadata = serde_json::from_slice(
            &fs::read(&metadata_file)
                .with_context(|| format!("failed to read {}", metadata_file.display()))?,
        )
        .with_context(|| {
            format!(
                "invalid browser bundle metadata {}",
                metadata_file.display()
            )
        })?;
        if metadata.schema_version != 1
            || metadata.bundler.name != "rolldown"
            || metadata.bundler.version.trim().is_empty()
        {
            bail!(
                "unsupported browser bundle metadata {}",
                metadata_file.display()
            );
        }
        let metadata_entry = metadata_project_file(&root, &metadata.entry_path, "bundle entry")?;
        if metadata_entry != entry {
            bail!("browser bundle entry does not match benchmark module");
        }

        let declared_sources = metadata.source_files.len();
        let source_files = metadata
            .source_files
            .iter()
            .map(|path| metadata_project_file(&root, path, "bundled module"))
            .collect::<Result<BTreeSet<_>>>()?;
        if source_files.is_empty() {
            bail!("browser bundle metadata contains no project source files");
        }
        if source_files.len() != declared_sources {
            bail!("browser bundle metadata contains duplicate project source files");
        }

        Ok(Self {
            bundle_file,
            metadata_file,
            source_files: source_files.into_iter().collect(),
        })
    }

    pub(crate) fn bundle_file(&self) -> &Path {
        &self.bundle_file
    }

    pub(crate) fn metadata_file(&self) -> &Path {
        &self.metadata_file
    }

    pub(crate) fn source_files(&self) -> &[PathBuf] {
        &self.source_files
    }

    pub(crate) fn identity_files(&self) -> [&Path; 2] {
        [&self.bundle_file, &self.metadata_file]
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BundleMetadata {
    schema_version: u32,
    bundler: BundlerIdentity,
    entry_path: String,
    source_files: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BundlerIdentity {
    name: String,
    version: String,
}

/// Bundle one benchmark entry for all browser engines and materialize its
/// immutable serving contract beneath `output_root`.
pub(crate) fn bundle(
    root: &Path,
    entry: &Path,
    output_root: &Path,
) -> Result<BrowserProjectBundle> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve benchmark root {}", root.display()))?;
    let entry = project_file(&root, entry, "benchmark module")?;
    fs::create_dir_all(output_root)
        .with_context(|| format!("failed to create bundle output {}", output_root.display()))?;
    let browser_sdk_path = materialize_browser_sdk(output_root)?;
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to initialize the project bundler")?;
    let (source, source_files) =
        runtime.block_on(bundle_source(&root, &entry, &browser_sdk_path))?;

    let bundle_file = output_root.join(BUNDLE_FILE);
    let metadata_file = output_root.join(METADATA_FILE);
    write_generated(&bundle_file, source.as_bytes())?;
    let metadata = BundleMetadata {
        schema_version: 1,
        bundler: BundlerIdentity {
            name: "rolldown".to_owned(),
            version: ROLLDOWN_VERSION.to_owned(),
        },
        entry_path: relative_path(&root, &entry)?,
        source_files: source_files
            .iter()
            .map(|path| relative_path(&root, path))
            .collect::<Result<_>>()?,
    };
    write_generated(
        &metadata_file,
        format!("{}\n", serde_json::to_string_pretty(&metadata)?).as_bytes(),
    )?;

    BrowserProjectBundle::open(&root, &entry, &bundle_file, &metadata_file)
}

fn materialize_browser_sdk(output_root: &Path) -> Result<PathBuf> {
    let source = BROWSER_SDK_SOURCE.as_bytes();
    let path = output_root.join(format!("bperf-browser-sdk-{:x}.ts", Sha256::digest(source)));
    if !path.exists() {
        let mut staged = tempfile::NamedTempFile::new_in(output_root)
            .context("failed to stage the embedded browser authoring module")?;
        staged
            .write_all(source)
            .context("failed to write the embedded browser authoring module")?;
        match staged.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error.error).with_context(|| {
                    format!(
                        "failed to persist the embedded browser authoring module {}",
                        path.display()
                    )
                });
            }
        }
    }
    let materialized = fs::read(&path)
        .with_context(|| format!("failed to read browser authoring module {}", path.display()))?;
    if materialized != source {
        bail!(
            "content-addressed browser authoring module is corrupt: {}",
            path.display()
        );
    }
    canonical_file(&path, "browser authoring module")
}

async fn bundle_source(
    root: &Path,
    entry: &Path,
    browser_sdk: &Path,
) -> Result<(String, Vec<PathBuf>)> {
    let entry_point = format!(
        "./{}",
        entry
            .strip_prefix(root)
            .context("benchmark entry is outside benchmark root")?
            .to_string_lossy()
            .replace('\\', "/")
    );
    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("benchmark".to_owned()),
            import: entry_point,
        }]),
        cwd: Some(root.to_owned()),
        platform: Some(Platform::Browser),
        format: Some(OutputFormat::Esm),
        sourcemap: Some(SourceMapType::Inline),
        sourcemap_path_transform: Some(source_map_path_transform(root, browser_sdk)),
        resolve: Some(ResolveOptions {
            alias: Some(vec![(
                "bperf/browser".to_owned(),
                vec![Some(portable_path(browser_sdk))],
            )]),
            ..Default::default()
        }),
        treeshake: TreeshakeOptions::Boolean(true),
        code_splitting: Some(CodeSplittingMode::Bool(false)),
        legal_comments: Some(LegalComments::None),
        experimental: Some(ExperimentalOptions {
            attach_debug_info: Some(AttachDebugInfo::None),
            ..Default::default()
        }),
        tsconfig: Some(TsConfig::Auto(true)),
        transform: Some(BundlerTransformOptions {
            target: Some(Either::Left("es2022".to_owned())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut bundler = BundlerBuilder::default()
        .with_options(options)
        .build()
        .context("failed to initialize Rolldown")?;
    let output = bundler
        .generate()
        .await
        .context("failed to bundle the browser benchmark with Rolldown")?;

    let [asset] = output.assets.as_slice() else {
        bail!("benchmark bundle must produce exactly one JavaScript output");
    };
    if !asset.filename().ends_with(".js") {
        bail!(
            "benchmark bundle produced an unexpected output {}",
            asset.filename()
        );
    }
    let source = String::from_utf8(asset.content_as_bytes().to_vec())
        .context("benchmark bundle output is not UTF-8")?;
    let watch_files = bundler
        .watch_files()
        .iter()
        .map(|entry| entry.key().to_string())
        .collect::<Vec<_>>();
    let source_files =
        collect_source_files(root, browser_sdk, watch_files.iter().map(String::as_str))?;
    bundler
        .close()
        .await
        .context("failed to close Rolldown after bundling")?;
    if source_files.is_empty() {
        bail!("benchmark bundle resolved no project source files");
    }
    Ok((source, source_files))
}

fn collect_source_files<'a>(
    root: &Path,
    browser_sdk: &Path,
    watch_files: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<PathBuf>> {
    let mut source_files = BTreeSet::new();
    for watched in watch_files {
        let path = Path::new(watched);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        if !path.is_file() {
            continue;
        }
        let resolved = canonical_file(&path, "bundled module")?;
        if resolved == browser_sdk {
            continue;
        }
        let source = project_file(root, &resolved, "bundled module")?;
        record_resolution_files(
            root,
            source.parent().context("bundled module has no parent")?,
            &mut source_files,
        )?;
        source_files.insert(source);
    }
    Ok(source_files.into_iter().collect())
}

fn source_map_path_transform(root: &Path, browser_sdk: &Path) -> SourceMapPathTransform {
    let root = root.to_owned();
    let browser_sdk = browser_sdk.to_owned();
    let browser_sdk_alias = PathBuf::from(portable_path(&browser_sdk));
    SourceMapPathTransform::new(Arc::new(move |source, _| {
        let source_path = Path::new(source);
        let transformed = if source_path == browser_sdk || source_path == browser_sdk_alias {
            "bperf/browser".to_owned()
        } else {
            source_path.strip_prefix(&root).map_or_else(
                |_| source.to_owned(),
                |relative| relative.to_string_lossy().replace('\\', "/"),
            )
        };
        Box::pin(async move { Ok(transformed) })
    }))
}

fn record_resolution_files(
    root: &Path,
    directory: &Path,
    source_files: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let mut current = canonical_project_path(root, directory, "bundled module directory")?;
    loop {
        for name in RESOLUTION_FILES {
            let candidate = current.join(name);
            if candidate.is_file() {
                source_files.insert(project_file(root, &candidate, "bundle resolution file")?);
            }
        }
        if current == root {
            break;
        }
        current = current
            .parent()
            .context("bundled module directory escaped benchmark root")?
            .to_owned();
    }
    Ok(())
}

/// Resolve an existing file after symlinks and reject paths outside the
/// canonical benchmark root.
pub(crate) fn project_file(root: &Path, target: &Path, label: &str) -> Result<PathBuf> {
    let resolved = canonical_project_path(root, target, label)?;
    if !resolved.is_file() {
        bail!("{label} is not a file: {}", target.display());
    }
    Ok(resolved)
}

fn canonical_project_path(root: &Path, target: &Path, label: &str) -> Result<PathBuf> {
    let resolved = fs::canonicalize(target)
        .with_context(|| format!("failed to resolve {label} {}", target.display()))?;
    if !resolved.starts_with(root) {
        bail!("{label} is outside benchmark root: {}", target.display());
    }
    Ok(resolved)
}

fn canonical_file(target: &Path, label: &str) -> Result<PathBuf> {
    let resolved = fs::canonicalize(target)
        .with_context(|| format!("failed to resolve {label} {}", target.display()))?;
    if !resolved.is_file() {
        bail!("{label} is not a file: {}", target.display());
    }
    Ok(resolved)
}

fn metadata_project_file(root: &Path, relative: &str, label: &str) -> Result<PathBuf> {
    if relative.trim().is_empty() || Path::new(relative).is_absolute() {
        bail!("{label} must be a non-empty project-relative path");
    }
    project_file(root, &root.join(relative), label)
}

fn relative_path(root: &Path, target: &Path) -> Result<String> {
    Ok(target
        .strip_prefix(root)
        .with_context(|| {
            format!(
                "project source {} is outside benchmark root {}",
                target.display(),
                root.display()
            )
        })?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn write_generated(path: &Path, content: &[u8]) -> Result<()> {
    if fs::read(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    bperf_storage::replace_file(path, content)
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolldown_bundles_the_browser_graph_and_records_resolution_inputs() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let output_root = root.join(".bperf");
        let source_root = root.join("src");
        let package_root = root.join("node_modules/example-package");
        let dependency_root = root.join("node_modules/example-dependency");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&package_root).unwrap();
        fs::create_dir_all(&dependency_root).unwrap();

        fs::write(
            root.join("package.json"),
            r#"{"private":true,"type":"module"}"#,
        )
        .unwrap();
        fs::write(root.join("package-lock.json"), "{}").unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        )
        .unwrap();
        fs::write(
            source_root.join("value.ts"),
            "export const typed: number = 40;\n",
        )
        .unwrap();
        fs::write(source_root.join("lazy.ts"), "export const lazy = 1;\n").unwrap();
        fs::write(
            package_root.join("package.json"),
            r#"{"name":"example-package","type":"module","exports":"./index.js"}"#,
        )
        .unwrap();
        fs::write(
            package_root.join("index.js"),
            "import dependency from 'example-dependency';\nexport const packageValue = dependency;\n",
        )
        .unwrap();
        fs::write(
            dependency_root.join("package.json"),
            r#"{"name":"example-dependency","main":"./index.cjs"}"#,
        )
        .unwrap();
        fs::write(dependency_root.join("index.cjs"), "module.exports = 2;\n").unwrap();
        let entry = root.join("sample.bench.ts");
        fs::write(
            &entry,
            [
                "import { exact } from 'bperf/browser';",
                "import { typed } from '@app/value';",
                "import { packageValue } from 'example-package';",
                "export const loadLazy = () => import('./src/lazy.ts');",
                "export default exact(typed + packageValue);",
            ]
            .join("\n"),
        )
        .unwrap();

        let bundle = bundle(root, &entry, &output_root).unwrap();
        let source = fs::read_to_string(bundle.bundle_file()).unwrap();
        assert!(!source.contains("bperf/browser"));
        assert!(source.contains("kind: \"exact\""));
        assert!(source.contains("packageValue"));
        assert!(source.contains("module.exports = 2"));
        assert!(source.contains("Promise.resolve"));
        assert!(source.contains("exact(40 + packageValue)"));
        assert!(source.contains("sourceMappingURL=data:application/json"));
        assert!(!source.contains(": number"));

        let source_files = bundle
            .source_files()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        for expected in [
            entry,
            source_root.join("value.ts"),
            source_root.join("lazy.ts"),
            package_root.join("package.json"),
            package_root.join("index.js"),
            dependency_root.join("package.json"),
            dependency_root.join("index.cjs"),
            root.join("package.json"),
            root.join("package-lock.json"),
            root.join("tsconfig.json"),
        ] {
            assert!(
                source_files.contains(&fs::canonicalize(&expected).unwrap()),
                "{}",
                expected.display()
            );
        }

        let metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(bundle.metadata_file()).unwrap()).unwrap();
        assert_eq!(metadata["bundler"]["name"], "rolldown");
        assert_eq!(metadata["bundler"]["version"], ROLLDOWN_VERSION);
    }

    #[test]
    fn benchmark_entry_must_be_inside_the_project_root() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let entry = outside.path().join("sample.bench.ts");
        fs::write(&entry, "export default 1;\n").unwrap();
        let output = project.path().join(".bperf");

        let error = bundle(project.path(), &entry, &output).unwrap_err();

        assert!(format!("{error:#}").contains("benchmark module is outside benchmark root"));
    }

    #[test]
    fn repeated_bundles_are_byte_for_byte_identical() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let output_root = root.join(".bperf");
        let entry = root.join("sample.bench.ts");
        fs::write(
            &entry,
            "import { exact } from 'bperf/browser';\nexport default exact(42);\n",
        )
        .unwrap();

        let first = bundle(root, &entry, &output_root).unwrap();
        let first_source = fs::read(first.bundle_file()).unwrap();
        let first_metadata = fs::read(first.metadata_file()).unwrap();
        let second = bundle(root, &entry, &output_root).unwrap();

        assert_eq!(fs::read(second.bundle_file()).unwrap(), first_source);
        assert_eq!(fs::read(second.metadata_file()).unwrap(), first_metadata);
    }
}
