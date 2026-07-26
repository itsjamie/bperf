//! Complete capture-file identity, validation, and Speedscope construction.
//!
//! Native payload construction and sample/frame selection remain engine-specific.
//! A prepared capture replaces exactly its three expected files and can finish
//! only after producing nonempty CPU, heap, and flamegraph evidence.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, ErrorKind, Read},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::lab::{ArtifactEvidence, ArtifactKind, Engine};

const REQUIRED_KINDS: [ArtifactKind; 3] = [
    ArtifactKind::CpuProfile,
    ArtifactKind::JsHeap,
    ArtifactKind::Flamegraph,
];

#[derive(Clone, Copy)]
struct ArtifactSpec {
    kind: ArtifactKind,
    name: &'static str,
    format: &'static str,
}

struct ArtifactLayout {
    cpu_profile: ArtifactSpec,
    heap_snapshot: ArtifactSpec,
    flamegraph: ArtifactSpec,
}

impl ArtifactLayout {
    const fn specs(&self) -> [ArtifactSpec; 3] {
        [self.cpu_profile, self.heap_snapshot, self.flamegraph]
    }
}

const CHROMIUM_LAYOUT: ArtifactLayout = ArtifactLayout {
    cpu_profile: ArtifactSpec {
        kind: ArtifactKind::CpuProfile,
        name: "chromium.cpu.cpuprofile",
        format: "V8 CPU profile",
    },
    heap_snapshot: ArtifactSpec {
        kind: ArtifactKind::JsHeap,
        name: "chromium.heap.heapsnapshot",
        format: "V8 heap snapshot",
    },
    flamegraph: ArtifactSpec {
        kind: ArtifactKind::Flamegraph,
        name: "chromium.flamegraph.speedscope.json",
        format: "Speedscope sampled profile",
    },
};

const FIREFOX_LAYOUT: ArtifactLayout = ArtifactLayout {
    cpu_profile: ArtifactSpec {
        kind: ArtifactKind::CpuProfile,
        name: "firefox.cpu.json",
        format: "Gecko Profiler JSON",
    },
    heap_snapshot: ArtifactSpec {
        kind: ArtifactKind::JsHeap,
        name: "firefox.heap.fxsnapshot",
        format: "Firefox .fxsnapshot",
    },
    flamegraph: ArtifactSpec {
        kind: ArtifactKind::Flamegraph,
        name: "firefox.flamegraph.speedscope.json",
        format: "Speedscope sampled profiles",
    },
};

const WEBKIT_LAYOUT: ArtifactLayout = ArtifactLayout {
    cpu_profile: ArtifactSpec {
        kind: ArtifactKind::CpuProfile,
        name: "webkit.cpu.json",
        format: "WebKit ScriptProfiler JSON",
    },
    heap_snapshot: ArtifactSpec {
        kind: ArtifactKind::JsHeap,
        name: "webkit.heap.json",
        format: "WebKit Heap snapshot JSON",
    },
    flamegraph: ArtifactSpec {
        kind: ArtifactKind::Flamegraph,
        name: "webkit.flamegraph.speedscope.json",
        format: "Speedscope sampled profile",
    },
};

fn layout(engine: Engine) -> &'static ArtifactLayout {
    match engine {
        Engine::Chromium => &CHROMIUM_LAYOUT,
        Engine::Firefox => &FIREFOX_LAYOUT,
        Engine::Webkit => &WEBKIT_LAYOUT,
    }
}

/// Prepares and completes the three immutable files required for one capture.
///
/// Existing files at the engine's expected paths are replaced before capture.
/// [`finish`](Self::finish) fails unless every required file is nonempty.
pub(crate) struct CaptureArtifacts {
    engine: Engine,
    root: PathBuf,
    layout: &'static ArtifactLayout,
}

impl CaptureArtifacts {
    pub(crate) fn prepare(engine: Engine, root: &Path) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| {
            format!(
                "failed to create {} artifact directory {}",
                engine,
                root.display()
            )
        })?;
        let root = fs::canonicalize(root)
            .with_context(|| format!("failed to resolve artifact directory {}", root.display()))?;
        let layout = layout(engine);
        for spec in layout.specs() {
            replace_existing(&root.join(spec.name))?;
        }
        Ok(Self {
            engine,
            root,
            layout,
        })
    }

    pub(crate) fn heap_snapshot_path(&self) -> PathBuf {
        self.root.join(self.layout.heap_snapshot.name)
    }

    pub(crate) fn write_cpu_profile(&self, contents: impl AsRef<[u8]>) -> Result<()> {
        self.write(self.layout.cpu_profile, contents.as_ref())
    }

    pub(crate) fn write_heap_snapshot(&self, contents: impl AsRef<[u8]>) -> Result<()> {
        self.write(self.layout.heap_snapshot, contents.as_ref())
    }

    pub(crate) fn write_flamegraph(&self, document: &SpeedscopeDocument) -> Result<()> {
        self.write(self.layout.flamegraph, &serde_json::to_vec(document)?)
    }

    pub(crate) fn finish(self) -> Result<Vec<ArtifactEvidence>> {
        self.layout
            .specs()
            .into_iter()
            .map(|spec| describe_artifact(self.engine, &self.root, spec))
            .collect()
    }

    fn write(&self, spec: ArtifactSpec, contents: &[u8]) -> Result<()> {
        if contents.is_empty() {
            bail!("{} emitted an empty {:?} artifact", self.engine, spec.kind);
        }
        let path = self.root.join(spec.name);
        fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
    }
}

fn replace_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            bail!("artifact path is a directory: {}", path.display())
        }
        Ok(_) => fs::remove_file(path)
            .with_context(|| format!("failed to replace artifact {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn describe_artifact(engine: Engine, root: &Path, spec: ArtifactSpec) -> Result<ArtifactEvidence> {
    let path = root.join(spec.name);
    let canonical_path =
        fs::canonicalize(&path).with_context(|| format!("failed to resolve {}", path.display()))?;
    if !canonical_path.starts_with(root) {
        bail!(
            "{engine} artifact escaped its artifact directory: {}",
            path.display()
        );
    }
    let size_bytes = fs::metadata(&canonical_path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .len();
    if size_bytes == 0 {
        bail!("{engine} emitted an empty artifact: {}", path.display());
    }
    Ok(ArtifactEvidence {
        kind: spec.kind,
        path: spec.name.to_owned(),
        size_bytes,
        sha256: sha256_file(&canonical_path)?,
        format: spec.format.to_owned(),
    })
}

pub(crate) fn validate_artifacts(
    engine: Engine,
    root: &Path,
    artifacts: &[ArtifactEvidence],
) -> Result<()> {
    validate_artifact_set(engine, artifacts)?;
    validate_artifact_files(engine, root, artifacts)
}

pub fn validate_artifact_set(engine: Engine, artifacts: &[ArtifactEvidence]) -> Result<()> {
    let expected_kinds = HashSet::from(REQUIRED_KINDS);
    let actual_kinds: HashSet<_> = artifacts.iter().map(|artifact| artifact.kind).collect();
    if actual_kinds != expected_kinds || artifacts.len() != REQUIRED_KINDS.len() {
        bail!("{engine} returned an invalid artifact set: {actual_kinds:?}");
    }

    for artifact in artifacts {
        let relative = Path::new(&artifact.path);
        if artifact.path.trim().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!(
                "{engine} artifact path must be contained: {}",
                artifact.path
            );
        }
        if artifact.size_bytes == 0
            || artifact.format.trim().is_empty()
            || !is_sha256(&artifact.sha256)
        {
            bail!("{engine} artifact descriptor is invalid: {}", artifact.path);
        }
    }
    Ok(())
}

pub fn validate_artifact_files(
    engine: Engine,
    root: &Path,
    artifacts: &[ArtifactEvidence],
) -> Result<()> {
    let canonical_root = fs::canonicalize(root)?;
    for artifact in artifacts {
        let relative = Path::new(&artifact.path);
        let canonical_path = fs::canonicalize(canonical_root.join(relative))
            .with_context(|| format!("artifact does not exist: {}", artifact.path))?;
        if !canonical_path.starts_with(&canonical_root) {
            bail!(
                "{engine} artifact escaped its artifact directory: {}",
                artifact.path
            );
        }
        let actual_size = fs::metadata(&canonical_path)?.len();
        if actual_size == 0 || actual_size != artifact.size_bytes {
            bail!(
                "{engine} artifact size mismatch for {}: reported {}, actual {}",
                artifact.path,
                artifact.size_bytes,
                actual_size
            );
        }
        if sha256_file(&canonical_path)? != artifact.sha256 {
            bail!("{engine} artifact hash mismatch for {}", artifact.path);
        }
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut digest = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        digest.update(&bytes[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct SpeedscopeFrame {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) col: Option<i64>,
}

impl SpeedscopeFrame {
    pub(crate) fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            file: None,
            line: None,
            col: None,
        }
    }
}

pub(crate) struct SpeedscopeBuilder {
    name: String,
    exporter: String,
    frames: Vec<SpeedscopeFrame>,
    frame_indexes: HashMap<SpeedscopeFrame, usize>,
    profiles: Vec<SpeedscopeProfile>,
}

impl SpeedscopeBuilder {
    pub(crate) fn new(name: impl Into<String>, exporter: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            exporter: exporter.into(),
            frames: Vec::new(),
            frame_indexes: HashMap::new(),
            profiles: Vec::new(),
        }
    }

    pub(crate) fn frame(&mut self, mut frame: SpeedscopeFrame) -> usize {
        if frame.name.is_empty() {
            frame.name = "(anonymous)".to_owned();
        }
        if let Some(index) = self.frame_indexes.get(&frame) {
            *index
        } else {
            let index = self.frames.len();
            self.frame_indexes.insert(frame.clone(), index);
            self.frames.push(frame);
            index
        }
    }

    pub(crate) fn sampled_profile(
        &mut self,
        name: impl Into<String>,
        unit: &'static str,
        start_value: f64,
        samples: Vec<Vec<usize>>,
        weights: Vec<f64>,
    ) -> Result<()> {
        let name = name.into();
        if samples.is_empty() || samples.len() != weights.len() {
            bail!("invalid Speedscope sample data for {name}");
        }
        if samples.iter().any(Vec::is_empty) {
            bail!("empty Speedscope stack in {name}");
        }
        if !start_value.is_finite()
            || weights
                .iter()
                .any(|weight| !weight.is_finite() || *weight <= 0.0)
        {
            bail!("non-positive Speedscope duration for {name}");
        }
        let duration = weights.iter().try_fold(0.0, |total, weight| {
            let next = total + weight;
            next.is_finite().then_some(next)
        });
        let duration =
            duration.with_context(|| format!("Speedscope duration overflowed for {name}"))?;
        self.profiles.push(SpeedscopeProfile {
            profile_type: "sampled",
            name,
            unit,
            start_value: CompactNumber(start_value),
            end_value: CompactNumber(start_value + duration),
            samples,
            weights: weights.into_iter().map(CompactNumber).collect(),
        });
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<SpeedscopeDocument> {
        if self.frames.is_empty() || self.profiles.is_empty() {
            bail!("no Speedscope data for {}", self.name);
        }
        Ok(SpeedscopeDocument {
            schema: "https://www.speedscope.app/file-format-schema.json",
            name: self.name,
            exporter: self.exporter,
            active_profile_index: 0,
            shared: SpeedscopeShared {
                frames: self.frames,
            },
            profiles: self.profiles,
        })
    }
}

pub(crate) fn positive_weights(timestamps: &[f64], fallback: f64) -> Result<Vec<f64>> {
    if !fallback.is_finite() || fallback <= 0.0 {
        bail!("Speedscope fallback weight must be positive");
    }
    if timestamps.iter().any(|timestamp| !timestamp.is_finite()) {
        bail!("Speedscope timestamps must be finite");
    }
    let mut deltas = timestamps
        .windows(2)
        .filter_map(|pair| {
            let delta = pair[1] - pair[0];
            (delta > 0.0).then_some(delta)
        })
        .collect::<Vec<_>>();
    deltas.sort_by(f64::total_cmp);
    let typical = deltas.get(deltas.len() / 2).copied().unwrap_or(fallback);
    Ok(timestamps
        .iter()
        .enumerate()
        .map(|(index, timestamp)| {
            timestamps.get(index + 1).map_or(typical, |next| {
                if next > timestamp {
                    next - timestamp
                } else {
                    typical
                }
            })
        })
        .collect())
}

#[derive(Serialize)]
pub(crate) struct SpeedscopeDocument {
    #[serde(rename = "$schema")]
    schema: &'static str,
    name: String,
    exporter: String,
    #[serde(rename = "activeProfileIndex")]
    active_profile_index: u32,
    shared: SpeedscopeShared,
    profiles: Vec<SpeedscopeProfile>,
}

#[derive(Serialize)]
struct SpeedscopeShared {
    frames: Vec<SpeedscopeFrame>,
}

#[derive(Serialize)]
struct SpeedscopeProfile {
    #[serde(rename = "type")]
    profile_type: &'static str,
    name: String,
    unit: &'static str,
    #[serde(rename = "startValue")]
    start_value: CompactNumber,
    #[serde(rename = "endValue")]
    end_value: CompactNumber,
    samples: Vec<Vec<usize>>,
    weights: Vec<CompactNumber>,
}

struct CompactNumber(f64);

impl Serialize for CompactNumber {
    fn serialize<SerializerType>(
        &self,
        serializer: SerializerType,
    ) -> std::result::Result<SerializerType::Ok, SerializerType::Error>
    where
        SerializerType: Serializer,
    {
        if self.0.fract() == 0.0 && self.0 >= i64::MIN as f64 && self.0 <= i64::MAX as f64 {
            serializer.serialize_i64(self.0 as i64)
        } else {
            serializer.serialize_f64(self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn test_flamegraph() -> SpeedscopeDocument {
        let mut builder = SpeedscopeBuilder::new("CPU", "bperf");
        let frame = builder.frame(SpeedscopeFrame::named("work"));
        builder
            .sampled_profile("main", "milliseconds", 0.0, vec![vec![frame]], vec![1.0])
            .unwrap();
        builder.finish().unwrap()
    }

    fn complete_artifacts(root: &Path, engine: Engine) -> Vec<ArtifactEvidence> {
        let artifacts = CaptureArtifacts::prepare(engine, root).unwrap();
        artifacts.write_cpu_profile(b"native cpu").unwrap();
        if engine == Engine::Webkit {
            artifacts.write_heap_snapshot(b"native heap").unwrap();
        } else {
            fs::write(artifacts.heap_snapshot_path(), b"native heap").unwrap();
        }
        artifacts.write_flamegraph(&test_flamegraph()).unwrap();
        artifacts.finish().unwrap()
    }

    #[test]
    fn complete_capture_artifacts_have_stable_identity_for_every_engine() {
        for (engine, expected) in [
            (
                Engine::Chromium,
                [
                    ("chromium.cpu.cpuprofile", "V8 CPU profile"),
                    ("chromium.heap.heapsnapshot", "V8 heap snapshot"),
                    (
                        "chromium.flamegraph.speedscope.json",
                        "Speedscope sampled profile",
                    ),
                ],
            ),
            (
                Engine::Firefox,
                [
                    ("firefox.cpu.json", "Gecko Profiler JSON"),
                    ("firefox.heap.fxsnapshot", "Firefox .fxsnapshot"),
                    (
                        "firefox.flamegraph.speedscope.json",
                        "Speedscope sampled profiles",
                    ),
                ],
            ),
            (
                Engine::Webkit,
                [
                    ("webkit.cpu.json", "WebKit ScriptProfiler JSON"),
                    ("webkit.heap.json", "WebKit Heap snapshot JSON"),
                    (
                        "webkit.flamegraph.speedscope.json",
                        "Speedscope sampled profile",
                    ),
                ],
            ),
        ] {
            let directory = tempdir().unwrap();
            let artifacts = complete_artifacts(directory.path(), engine);
            assert_eq!(
                artifacts
                    .iter()
                    .map(|artifact| (artifact.path.as_str(), artifact.format.as_str()))
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                artifacts
                    .iter()
                    .map(|artifact| artifact.kind)
                    .collect::<Vec<_>>(),
                REQUIRED_KINDS
            );
            validate_artifacts(engine, directory.path(), &artifacts).unwrap();
        }
    }

    #[test]
    fn preparing_a_capture_replaces_only_its_expected_files() {
        let directory = tempdir().unwrap();
        let artifacts = complete_artifacts(directory.path(), Engine::Firefox);
        let unrelated = directory.path().join("keep.txt");
        fs::write(&unrelated, b"keep").unwrap();

        CaptureArtifacts::prepare(Engine::Firefox, directory.path()).unwrap();

        for artifact in artifacts {
            assert!(!directory.path().join(artifact.path).exists());
        }
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    }

    #[test]
    fn incomplete_and_empty_captures_fail_at_the_artifact_interface() {
        let directory = tempdir().unwrap();
        let artifacts = CaptureArtifacts::prepare(Engine::Chromium, directory.path()).unwrap();
        assert!(artifacts.write_cpu_profile([]).is_err());
        artifacts.write_cpu_profile(b"native cpu").unwrap();
        artifacts.write_flamegraph(&test_flamegraph()).unwrap();
        assert!(artifacts.finish().is_err());
    }

    #[test]
    fn validation_rejects_tampering_and_uncontained_descriptors() {
        let directory = tempdir().unwrap();
        let mut artifacts = complete_artifacts(directory.path(), Engine::Webkit);
        fs::write(
            directory.path().join(&artifacts[0].path),
            b"tampered native cpu",
        )
        .unwrap();
        assert!(validate_artifacts(Engine::Webkit, directory.path(), &artifacts).is_err());

        artifacts[0].path = "../outside.json".to_owned();
        assert!(validate_artifact_set(Engine::Webkit, &artifacts).is_err());
    }

    #[test]
    fn sampled_profiles_intern_frames_and_normalize_numbers() {
        let mut builder = SpeedscopeBuilder::new("CPU", "bperf");
        let frame = builder.frame(SpeedscopeFrame::named("work"));
        assert_eq!(builder.frame(SpeedscopeFrame::named("work")), frame);
        builder
            .sampled_profile(
                "main",
                "milliseconds",
                0.0,
                vec![vec![frame], vec![frame]],
                vec![2.0, 0.5],
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(builder.finish().unwrap()).unwrap(),
            json!({
                "$schema": "https://www.speedscope.app/file-format-schema.json",
                "name": "CPU",
                "exporter": "bperf",
                "activeProfileIndex": 0,
                "shared": {"frames": [{"name": "work"}]},
                "profiles": [{
                    "type": "sampled",
                    "name": "main",
                    "unit": "milliseconds",
                    "startValue": 0,
                    "endValue": 2.5,
                    "samples": [[0], [0]],
                    "weights": [2, 0.5],
                }],
            })
        );
    }

    #[test]
    fn positive_weights_use_the_median_delta_for_terminal_samples() {
        assert_eq!(
            positive_weights(&[0.0, 2.0, 5.0], 1.0).unwrap(),
            [2.0, 3.0, 3.0]
        );
    }
}
