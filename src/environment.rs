//! Browser/runtime identity and fresh performance anchors for measurement sets.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    browser_lab::{
        BrowserLab, CaptureEvidence, Engine, PROTOCOL_VERSION, RuntimeAnchorEvidence,
        RuntimeEvidence,
    },
    measurement::{self, MeasurementSet},
};

const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BrowserBuild {
    executable_path: String,
    version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentIdentity {
    bperf_version: String,
    browser_lab_protocol_version: u32,
    runtime: RuntimeEvidence,
    browsers: BTreeMap<String, BrowserBuild>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentRecord {
    schema_version: u32,
    recorded_at_unix_ms: u64,
    fingerprint: String,
    identity: EnvironmentIdentity,
    anchors: BTreeMap<String, RuntimeAnchorEvidence>,
}

impl EnvironmentRecord {
    pub(crate) fn recorded_at_unix_ms(&self) -> u64 {
        self.recorded_at_unix_ms
    }

    pub(crate) fn anchor(&self, engine: Engine) -> Result<&[f64]> {
        self.anchors
            .get(engine.as_str())
            .map(|anchor| anchor.wall_ms.as_slice())
            .with_context(|| format!("environment record has no {engine} runtime anchor"))
    }
}

pub(crate) struct EnvironmentPair {
    pub(crate) baseline: EnvironmentRecord,
    pub(crate) candidate: EnvironmentRecord,
}

pub(crate) fn capture(
    browser_lab: &mut BrowserLab,
    measurement: &MeasurementSet,
) -> Result<String> {
    let run_root = measurement.preflight_capture_root().join(unique_id("run")?);
    let mut captures = Vec::new();
    for engine in measurement.benchmark.engines() {
        eprintln!("[measure] preflight {engine}");
        captures.push(browser_lab.probe(*engine, &run_root)?);
    }

    let identity = environment_identity(&captures)?;
    let fingerprint = environment_fingerprint(&identity)?;
    let anchors = captures
        .iter()
        .map(|capture| (capture.engine.as_str().to_owned(), capture.anchor.clone()))
        .collect();
    let record = EnvironmentRecord {
        schema_version: SCHEMA_VERSION,
        recorded_at_unix_ms: unix_time_ms()?,
        fingerprint,
        identity,
        anchors,
    };
    validate_record(&record)?;

    let path = measurement.root().join("environment.json");
    if path.exists() {
        let existing = read_path(&path)?;
        if existing.identity != record.identity || existing.fingerprint != record.fingerprint {
            bail!("current preflight does not match {}", path.display());
        }
        return Ok(existing.fingerprint);
    }
    measurement::write_immutable(
        &path,
        format!("{}\n", serde_json::to_string_pretty(&record)?).as_bytes(),
    )?;
    Ok(record.fingerprint)
}

pub(crate) fn compatible_pair(
    baseline: &MeasurementSet,
    candidate: &MeasurementSet,
) -> Result<Option<EnvironmentPair>> {
    let (Some(baseline_fingerprint), Some(candidate_fingerprint)) = (
        baseline.environment_fingerprint(),
        candidate.environment_fingerprint(),
    ) else {
        return Ok(None);
    };
    let baseline_record = read(baseline)?;
    let candidate_record = read(candidate)?;
    if baseline_fingerprint != candidate_fingerprint
        || baseline_record.identity != candidate_record.identity
    {
        bail!(
            "measurement sets use different pinned browser/runtime identities and cannot be compared"
        );
    }
    Ok(Some(EnvironmentPair {
        baseline: baseline_record,
        candidate: candidate_record,
    }))
}

pub(crate) fn read(measurement: &MeasurementSet) -> Result<EnvironmentRecord> {
    let path = measurement.root().join("environment.json");
    let record = read_path(&path)?;
    if measurement
        .environment_fingerprint()
        .is_some_and(|fingerprint| fingerprint != record.fingerprint)
    {
        bail!(
            "environment record {} does not match its trial evidence",
            path.display()
        );
    }
    Ok(record)
}

fn read_path(path: &Path) -> Result<EnvironmentRecord> {
    let record: EnvironmentRecord = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| {
        format!(
            "invalid {}; remeasure with the current runtime-anchor protocol",
            path.display()
        )
    })?;
    validate_record(&record)
        .with_context(|| format!("invalid environment record {}", path.display()))?;
    Ok(record)
}

fn validate_record(record: &EnvironmentRecord) -> Result<()> {
    if record.schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported environment schema {}; expected {}",
            record.schema_version,
            SCHEMA_VERSION
        );
    }
    if record.recorded_at_unix_ms == 0 {
        bail!("environment record has no capture time");
    }
    if environment_fingerprint(&record.identity)? != record.fingerprint {
        bail!("environment fingerprint does not match its identity");
    }
    let expected: BTreeSet<_> = record.identity.browsers.keys().collect();
    let actual: BTreeSet<_> = record.anchors.keys().collect();
    if expected != actual || actual.is_empty() {
        bail!("runtime anchors do not match the pinned browser set");
    }
    for (engine, anchor) in &record.anchors {
        anchor
            .validate()
            .with_context(|| format!("{engine} runtime anchor is invalid"))?;
    }
    Ok(())
}

fn environment_identity(captures: &[CaptureEvidence]) -> Result<EnvironmentIdentity> {
    let first = captures.first().context("preflight returned no engines")?;
    let runtime = first.runtime.clone();
    let mut browsers = BTreeMap::new();
    for capture in captures {
        if capture.runtime != runtime {
            bail!("preflight engines used different Node or Playwright runtimes");
        }
        browsers.insert(
            capture.engine.as_str().to_owned(),
            BrowserBuild {
                executable_path: capture.browser.executable_path.clone(),
                version: capture.browser.version.clone(),
            },
        );
    }
    Ok(EnvironmentIdentity {
        bperf_version: env!("CARGO_PKG_VERSION").to_owned(),
        browser_lab_protocol_version: PROTOCOL_VERSION,
        runtime,
        browsers,
    })
}

fn environment_fingerprint(identity: &EnvironmentIdentity) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"bperf-browser-environment-v1\0");
    digest.update(serde_json::to_vec(identity)?);
    Ok(format!("{:x}", digest.finalize()))
}

fn unique_id(prefix: &str) -> Result<String> {
    Ok(format!(
        "{prefix}-{}-{}",
        unix_time_ms()?,
        std::process::id()
    ))
}

fn unix_time_ms() -> Result<u64> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(milliseconds).context("system time does not fit in environment timestamp")
}
