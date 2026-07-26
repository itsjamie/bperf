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

const SCHEMA_VERSION: u32 = 5;

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

    if let Some(existing) = measurement.environment_record::<EnvironmentRecord>()? {
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
    let record = measurement
        .environment_record::<EnvironmentRecord>()?
        .context("measurement set has no environment record")?;
    if measurement
        .environment_fingerprint()
        .is_some_and(|fingerprint| fingerprint != record.fingerprint)
    {
        bail!("environment record does not match its trial evidence");
    }
    Ok(record)
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
            "unsupported environment schema {schema}; expected {SCHEMA_VERSION}; measurements made with former Node-owned browser adapters must be remeasured"
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
    if environment_fingerprint(&record.identity)? != record.fingerprint {
        bail!("environment fingerprint does not match its identity");
    }
    let expected: BTreeSet<_> = record.identity.browsers.keys().collect();
    let adapters: BTreeSet<_> = record.identity.adapters.keys().collect();
    let actual: BTreeSet<_> = record.anchors.keys().collect();
    if expected != actual || adapters != actual || actual.is_empty() {
        bail!("runtime anchors and adapters do not match the pinned browser set");
    }
    for (engine, anchor) in &record.anchors {
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
    digest.update(b"bperf-browser-environment-v4\0");
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

#[cfg(unix)]
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
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::read_path;

    #[test]
    fn former_node_browser_environments_require_remeasurement() {
        for schema_version in [2, 3, 4] {
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
            assert!(error.contains("former Node-owned browser adapters"));
            assert!(error.contains("remeasured"));
        }
    }
}
