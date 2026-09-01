//! Browser/runtime identity and fresh performance anchors for measurement sets.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(any(unix, test))]
use std::fs;

use anyhow::{Context, Result, bail};
use bperf_browser::lab::{
    AdapterEvidence, BrowserLab, CaptureEvidence, Engine, PROTOCOL_VERSION, RuntimeAnchorEvidence,
};
use bperf_measurement::store::MeasurementSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: u32 = 7;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BrowserBuild {
    executable_path: String,
    version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HostIdentity {
    platform: String,
    arch: String,
    os_release: String,
    cpu_model: String,
    logical_cpus: u32,
    total_memory_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentIdentity {
    bperf_version: String,
    browser_lab_protocol_version: u32,
    host: HostIdentity,
    adapters: BTreeMap<String, AdapterEvidence>,
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

/// Stable host and browser identity suitable for human-facing evidence views.
///
/// Protocol versions, executable paths, and anchor payloads remain hidden in
/// the environment record; callers receive only the identity needed to
/// distinguish comparable measurement sets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EnvironmentSummary {
    pub recorded_at_unix_ms: u64,
    pub fingerprint: String,
    pub platform: String,
    pub arch: String,
    pub os_release: String,
    pub browser_versions: BTreeMap<Engine, String>,
}

impl EnvironmentSummary {
    pub(crate) fn validate(&self, expected_fingerprint: &str) -> Result<()> {
        if self.recorded_at_unix_ms == 0
            || self.fingerprint != expected_fingerprint
            || [&self.platform, &self.arch, &self.os_release]
                .into_iter()
                .any(|value| value.trim().is_empty())
            || self.browser_versions.len() != Engine::ALL.len()
            || Engine::ALL.into_iter().any(|engine| {
                self.browser_versions
                    .get(&engine)
                    .is_none_or(|version| version.trim().is_empty())
            })
        {
            bail!("persisted environment summary is incomplete or inconsistent");
        }
        Ok(())
    }
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

pub fn summary(measurement: &MeasurementSet) -> Result<EnvironmentSummary> {
    let record = read(measurement)?;
    let browser_versions = Engine::ALL
        .into_iter()
        .map(|engine| {
            let version = record
                .identity
                .browsers
                .get(engine.as_str())
                .with_context(|| format!("environment record has no {engine} browser identity"))?
                .version
                .clone();
            Ok((engine, version))
        })
        .collect::<Result<_>>()?;
    Ok(EnvironmentSummary {
        recorded_at_unix_ms: record.recorded_at_unix_ms,
        fingerprint: record.fingerprint,
        platform: record.identity.host.platform,
        arch: record.identity.host.arch,
        os_release: record.identity.host.os_release,
        browser_versions,
    })
}

pub(crate) struct EnvironmentPair {
    pub(crate) baseline: EnvironmentRecord,
    pub(crate) candidate: EnvironmentRecord,
}

pub fn capture(browser_lab: &mut BrowserLab, measurement: &MeasurementSet) -> Result<String> {
    let run_root = measurement.preflight_run_root(&unique_id("run")?);
    let mut captures = Vec::new();
    for engine in measurement.benchmark().engines() {
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

    if let Some(existing) = stored_record(measurement)? {
        if existing.identity != record.identity || existing.fingerprint != record.fingerprint {
            bail!("current preflight does not match the recorded environment");
        }
        return Ok(existing.fingerprint);
    }
    measurement.write_environment_record(&record)?;
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
    stored_record(measurement)?.context("measurement set has no environment record")
}

fn stored_record(measurement: &MeasurementSet) -> Result<Option<EnvironmentRecord>> {
    let Some(record) = measurement.environment_record::<EnvironmentRecord>()? else {
        return Ok(None);
    };
    validate_record(&record).with_context(|| {
        format!(
            "measurement set {} has an invalid environment record",
            measurement.measurement_set_id()
        )
    })?;
    let requested: BTreeSet<_> = measurement
        .benchmark()
        .engines()
        .iter()
        .map(|engine| engine.as_str())
        .collect();
    let recorded: BTreeSet<_> = record
        .identity
        .browsers
        .keys()
        .map(String::as_str)
        .collect();
    if recorded != requested {
        bail!("environment record does not cover the measurement's requested engines");
    }
    if measurement
        .environment_fingerprint()
        .is_some_and(|fingerprint| fingerprint != record.fingerprint)
    {
        bail!("environment record does not match its trial evidence");
    }
    Ok(Some(record))
}

#[cfg(test)]
fn read_path(path: &std::path::Path) -> Result<EnvironmentRecord> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "invalid {}; remeasure with the current runtime-anchor protocol",
            path.display()
        )
    })?;
    let schema = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if schema != u64::from(SCHEMA_VERSION) {
        bail!(
            "unsupported environment schema {schema}; expected {SCHEMA_VERSION}; measurements made with earlier browser capture contracts must be remeasured"
        );
    }
    let record: EnvironmentRecord = serde_json::from_value(value).with_context(|| {
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
    if record.identity.bperf_version.trim().is_empty()
        || record.identity.browser_lab_protocol_version != PROTOCOL_VERSION
        || record.identity.host.platform.trim().is_empty()
        || record.identity.host.arch.trim().is_empty()
        || record.identity.host.os_release.trim().is_empty()
        || record.identity.host.cpu_model.trim().is_empty()
        || record.identity.host.logical_cpus == 0
        || record.identity.host.total_memory_bytes == 0
    {
        bail!("environment identity is incomplete or uses an unsupported protocol");
    }
    if environment_fingerprint(&record.identity)? != record.fingerprint {
        bail!("environment fingerprint does not match its identity");
    }
    let expected: BTreeSet<_> = record.identity.browsers.keys().collect();
    let adapters: BTreeSet<_> = record.identity.adapters.keys().collect();
    let actual: BTreeSet<_> = record.anchors.keys().collect();
    if expected != actual || adapters != actual || actual.is_empty() {
        bail!("runtime anchors and adapters do not match the pinned browser set");
    }
    for (engine_name, anchor) in &record.anchors {
        let engine = Engine::ALL
            .into_iter()
            .find(|engine| engine.as_str() == engine_name)
            .with_context(|| format!("environment record has unknown engine {engine_name:?}"))?;
        record.identity.adapters[engine_name]
            .validate_for(engine)
            .with_context(|| format!("{engine} adapter identity is invalid"))?;
        let browser = &record.identity.browsers[engine_name];
        if browser.executable_path.trim().is_empty() || browser.version.trim().is_empty() {
            bail!("{engine} browser identity is incomplete");
        }
        anchor
            .validate()
            .with_context(|| format!("{engine} runtime anchor is invalid"))?;
    }
    Ok(())
}

fn environment_identity(captures: &[CaptureEvidence]) -> Result<EnvironmentIdentity> {
    if captures.is_empty() {
        bail!("preflight returned no engines");
    }
    let mut browsers = BTreeMap::new();
    let mut adapters = BTreeMap::new();
    for capture in captures {
        let engine = capture.engine.as_str().to_owned();
        if browsers
            .insert(
                engine.clone(),
                BrowserBuild {
                    executable_path: capture.browser.executable_path.clone(),
                    version: capture.browser.version.clone(),
                },
            )
            .is_some()
        {
            bail!("preflight returned duplicate {} evidence", capture.engine);
        }
        adapters.insert(engine, capture.adapter.clone());
    }
    Ok(EnvironmentIdentity {
        bperf_version: env!("CARGO_PKG_VERSION").to_owned(),
        browser_lab_protocol_version: PROTOCOL_VERSION,
        host: host_identity()?,
        adapters,
        browsers,
    })
}

fn environment_fingerprint(identity: &EnvironmentIdentity) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"bperf-browser-environment-v5\0");
    digest.update(serde_json::to_vec(identity)?);
    Ok(format!("{:x}", digest.finalize()))
}

fn host_identity() -> Result<HostIdentity> {
    Ok(HostIdentity {
        platform: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        os_release: os_release()?,
        cpu_model: cpu_model()?,
        logical_cpus: u32::try_from(
            std::thread::available_parallelism()
                .context("failed to determine logical CPU count")?
                .get(),
        )
        .context("logical CPU count does not fit environment identity")?,
        total_memory_bytes: total_memory_bytes()?,
    })
}

fn command_output(program: &str, arguments: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .with_context(|| format!("failed to run {program} for host identity"))?;
    if !output.status.success() {
        bail!("{program} failed while capturing host identity");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(windows)]
fn os_release() -> Result<String> {
    command_output("cmd", &["/C", "ver"])
}

#[cfg(target_os = "linux")]
fn os_release() -> Result<String> {
    let release = fs::read_to_string("/etc/os-release")
        .context("failed to read /etc/os-release for host identity")?;
    linux_os_release(&release, &command_output("uname", &["-r"])?)
}

#[cfg(any(target_os = "linux", test))]
fn linux_os_release(release: &str, kernel: &str) -> Result<String> {
    let field = |name| {
        release.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key == name).then(|| {
                value
                    .trim()
                    .trim_matches(|character| character == '"' || character == '\'')
            })
        })
    };
    let distribution = field("ID")
        .filter(|value| !value.is_empty())
        .context("/etc/os-release has no distribution ID")?;
    let version = field("VERSION_ID")
        .filter(|value| !value.is_empty())
        .or_else(|| field("BUILD_ID"))
        .filter(|value| !value.is_empty());
    Ok(format!(
        "{distribution}{} (kernel {kernel})",
        version.map_or_else(String::new, |version| format!(" {version}"))
    ))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn os_release() -> Result<String> {
    command_output("uname", &["-r"])
}

#[cfg(windows)]
fn cpu_model() -> Result<String> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr::null_mut};

    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
    };

    let subkey: Vec<u16> = std::ffi::OsStr::new(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let name: Vec<u16> = std::ffi::OsStr::new("ProcessorNameString")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut byte_count = 0_u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut byte_count,
        )
    };
    if status != ERROR_SUCCESS || byte_count < 2 {
        return std::env::var("PROCESSOR_IDENTIFIER")
            .context("Windows CPU model is unavailable for host identity");
    }
    let mut buffer = vec![0_u16; byte_count as usize / 2];
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast::<c_void>(),
            &mut byte_count,
        )
    };
    if status != ERROR_SUCCESS {
        bail!("failed to read the Windows CPU model from the registry");
    }
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    let model = String::from_utf16(&buffer[..length])?.trim().to_owned();
    if model.is_empty() {
        bail!("Windows CPU model is empty");
    }
    Ok(model)
}

#[cfg(target_os = "macos")]
fn cpu_model() -> Result<String> {
    command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
        .or_else(|_| command_output("sysctl", &["-n", "hw.model"]))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn cpu_model() -> Result<String> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").context("failed to read /proc/cpuinfo")?;
    cpuinfo
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            ["model name", "hardware"]
                .iter()
                .any(|candidate| name.trim().eq_ignore_ascii_case(candidate))
                .then(|| value.trim().to_owned())
        })
        .filter(|value| !value.is_empty())
        .context("/proc/cpuinfo has no CPU model")
}

#[cfg(windows)]
fn total_memory_bytes() -> Result<u64> {
    use std::mem::{size_of, zeroed};

    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status: MEMORYSTATUSEX = unsafe { zeroed() };
    status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read host memory");
    }
    Ok(status.ullTotalPhys)
}

#[cfg(target_os = "macos")]
fn total_memory_bytes() -> Result<u64> {
    command_output("sysctl", &["-n", "hw.memsize"])?
        .parse()
        .context("macOS physical memory is not numeric")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn total_memory_bytes() -> Result<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").context("failed to read /proc/meminfo")?;
    let kibibytes: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.split_whitespace().next())
        .context("/proc/meminfo has no MemTotal")?
        .parse()
        .context("/proc/meminfo MemTotal is not numeric")?;
    kibibytes
        .checked_mul(1024)
        .context("host physical memory size overflowed")
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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use bperf_measurement::store::{MeasurementSet, prepare};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn earlier_browser_environments_require_remeasurement() {
        for schema_version in [2, 3, 4, 5, 6] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("environment.json");
            fs::write(
                &path,
                serde_json::to_vec(&json!({
                    "schema_version": schema_version,
                    "recorded_at_unix_ms": 1
                }))
                .unwrap(),
            )
            .unwrap();

            let error = read_path(&path).unwrap_err().to_string();
            assert!(error.contains("earlier browser capture contracts"));
            assert!(error.contains("remeasured"));
        }
    }

    #[test]
    fn linux_host_identity_distinguishes_userlands_on_the_same_kernel() {
        let kernel = "6.8.0-test";
        let ubuntu = linux_os_release("ID=ubuntu\nVERSION_ID=\"24.04\"\n", kernel).unwrap();
        let debian = linux_os_release("ID=debian\nVERSION_ID=13\n", kernel).unwrap();

        assert_eq!(ubuntu, "ubuntu 24.04 (kernel 6.8.0-test)");
        assert_eq!(debian, "debian 13 (kernel 6.8.0-test)");
        assert_ne!(ubuntu, debian);
        assert_eq!(
            linux_os_release("ID=arch\nVERSION_ID=\nBUILD_ID=rolling\n", kernel).unwrap(),
            "arch rolling (kernel 6.8.0-test)"
        );
    }

    #[test]
    fn persisted_environment_records_are_validated_on_read() {
        let directory = tempdir().unwrap();
        let root = prepare(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            Some(20),
            directory.path(),
        )
        .unwrap();
        let measurement = MeasurementSet::open(&root).unwrap();
        measurement
            .write_environment_record(&EnvironmentRecord {
                schema_version: SCHEMA_VERSION - 1,
                recorded_at_unix_ms: 1,
                fingerprint: "obsolete".to_owned(),
                identity: EnvironmentIdentity {
                    bperf_version: "test".to_owned(),
                    browser_lab_protocol_version: PROTOCOL_VERSION,
                    host: HostIdentity {
                        platform: "test".to_owned(),
                        arch: "test".to_owned(),
                        os_release: "test".to_owned(),
                        cpu_model: "test".to_owned(),
                        logical_cpus: 1,
                        total_memory_bytes: 1,
                    },
                    adapters: BTreeMap::new(),
                    browsers: BTreeMap::new(),
                },
                anchors: BTreeMap::new(),
            })
            .unwrap();

        let error = read(&measurement).unwrap_err();
        assert!(
            format!("{error:#}").contains("unsupported environment schema"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn persisted_environments_must_cover_every_requested_engine() {
        let directory = tempdir().unwrap();
        let root = prepare(
            &example("browser-benchmark.yaml"),
            &example("browser-variant-baseline.yaml"),
            Some(20),
            directory.path(),
        )
        .unwrap();
        let measurement = MeasurementSet::open(&root).unwrap();
        let engine = Engine::Chromium;
        let identity = EnvironmentIdentity {
            bperf_version: "test".to_owned(),
            browser_lab_protocol_version: PROTOCOL_VERSION,
            host: HostIdentity {
                platform: "test".to_owned(),
                arch: "test".to_owned(),
                os_release: "test".to_owned(),
                cpu_model: "test".to_owned(),
                logical_cpus: 1,
                total_memory_bytes: 1,
            },
            adapters: BTreeMap::from([(
                engine.as_str().to_owned(),
                AdapterEvidence::Chromium {
                    playwright: "test".to_owned(),
                    chromium_revision: "test".to_owned(),
                    executable_sha256: "a".repeat(64),
                    protocol_version: 1,
                    browser_workload_version: 1,
                },
            )]),
            browsers: BTreeMap::from([(
                engine.as_str().to_owned(),
                BrowserBuild {
                    executable_path: "/test/chromium".to_owned(),
                    version: "test".to_owned(),
                },
            )]),
        };
        let fingerprint = environment_fingerprint(&identity).unwrap();
        measurement
            .write_environment_record(&EnvironmentRecord {
                schema_version: SCHEMA_VERSION,
                recorded_at_unix_ms: 1,
                fingerprint,
                identity,
                anchors: BTreeMap::from([(
                    engine.as_str().to_owned(),
                    RuntimeAnchorEvidence {
                        workload: "javascript_cpu_v1".to_owned(),
                        wall_ms: vec![1.0; 31],
                        batch_size: 1,
                        checksum: 1,
                    },
                )]),
            })
            .unwrap();

        let error = read(&measurement).unwrap_err();
        assert!(
            format!("{error:#}").contains("does not cover the measurement's requested engines"),
            "unexpected error: {error:#}"
        );
    }

    fn example(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("examples")
            .join(name)
    }
}
