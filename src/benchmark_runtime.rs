//! Execution of a prepared benchmark against one immutable measurement set.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    ops::{Deref, DerefMut},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::{
    ArtifactEvidence, BrowserLab, BrowserTrialConfig, BrowserTrialRequest, Engine,
};
use bperf_measurement::{
    MEASUREMENT_SCHEMA_VERSION,
    manifest::{VariantInvocation, VerifierInvocation},
    schedule::{ScheduledTrial, TrialPhase},
    store::{MeasurementSet, TrialResult},
};
use serde::{Deserialize, Serialize};

const READINESS_PROTOCOL_VERSION: u32 = 2;
const MAX_ADAPTER_READINESS_BYTES: usize = 1024 * 1024;
const MAX_VERIFIER_OUTPUT_BYTES: usize = 1024 * 1024;

/// Frozen workload inputs and invocation rules for repeated isolated trials.
///
/// Successful execution includes correctness verification and measurement-local
/// artifact paths. Transport and verifier protocols do not escape this
/// boundary.
pub(crate) struct BenchmarkRuntime {
    workloads: BTreeMap<String, Vec<serde_json::Value>>,
    browser_config: BrowserTrialConfig,
    adapter: VariantAdapter,
}

impl BenchmarkRuntime {
    pub(crate) fn prepare(measurement: &MeasurementSet) -> Result<Self> {
        Ok(Self {
            workloads: load_workloads(measurement)?,
            browser_config: measurement.benchmark().browser_trial_config(),
            adapter: VariantAdapter::start(&measurement.variant().invocation())?,
        })
    }

    pub(crate) fn execute_trial(
        &self,
        measurement: &MeasurementSet,
        browser_lab: &mut BrowserLab,
        trial: &ScheduledTrial,
        attempt: u32,
        environment_fingerprint: &str,
    ) -> Result<TrialResult> {
        let operations = self
            .workloads
            .get(&trial.workload_id)
            .with_context(|| format!("workload {} was not loaded", trial.workload_id))?;
        let artifact_root = measurement
            .root()
            .join("artifacts")
            .join(&trial.trial_id)
            .join(format!("attempt-{attempt:04}"));
        let mut evidence = browser_lab.measure_trial(BrowserTrialRequest {
            engine: trial.engine,
            artifact_root: &artifact_root,
            target_url: self.adapter.url(),
            operations,
            browser: &self.browser_config,
            batches: measurement.trial_batches(trial),
        })?;
        let verdict = verify(measurement, trial, operations, &evidence.workload.result)?;
        make_artifact_paths_measurement_relative(
            measurement.root(),
            &artifact_root,
            &mut evidence.artifacts,
        )?;
        Ok(TrialResult {
            schema_version: MEASUREMENT_SCHEMA_VERSION,
            measurement_set_id: measurement.measurement_set_id().to_owned(),
            trial_id: trial.trial_id.clone(),
            attempt,
            workload_id: trial.workload_id.clone(),
            engine: trial.engine,
            phase: trial.phase,
            sample_index: trial.sample_index,
            environment_fingerprint: environment_fingerprint.to_owned(),
            valid: true,
            success: verdict.success,
            failure_category: verdict.failure_category,
            failure_detail: verdict.detail,
            invalidation_reason: None,
            metrics: evidence.metrics,
            artifacts: evidence.artifacts,
        })
    }
}

fn load_workloads(
    measurement: &MeasurementSet,
) -> Result<BTreeMap<String, Vec<serde_json::Value>>> {
    let mut workloads = BTreeMap::new();
    for id in measurement.benchmark().workload_ids() {
        let workload = measurement
            .benchmark()
            .workload(id)
            .with_context(|| format!("workload {id:?} disappeared"))?;
        let source = fs::read_to_string(workload.trace_file).with_context(|| {
            format!(
                "failed to read workload trace {}",
                workload.trace_file.display()
            )
        })?;
        let mut operations = Vec::new();
        for (line_index, line) in source.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            operations.push(serde_json::from_str(line).with_context(|| {
                format!(
                    "invalid JSON in {} at line {}",
                    workload.trace_file.display(),
                    line_index + 1
                )
            })?);
        }
        if operations.is_empty() {
            bail!("workload {id:?} contains no operations");
        }
        let frozen = format!("{}\n", serde_json::to_string_pretty(&operations)?);
        measurement.freeze_workload(id, frozen.as_bytes())?;
        workloads.insert(id.to_owned(), operations);
    }
    Ok(workloads)
}

#[derive(Serialize)]
struct VerifierPayload<'a> {
    schema_version: u32,
    benchmark_id: &'a str,
    subject_id: &'a str,
    variant_id: &'a str,
    workload_id: &'a str,
    engine: Engine,
    phase: TrialPhase,
    sample_index: u32,
    operations: &'a [serde_json::Value],
    workload_result: &'a [serde_json::Value],
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifierVerdict {
    success: bool,
    #[serde(default)]
    failure_category: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

fn verify(
    measurement: &MeasurementSet,
    trial: &ScheduledTrial,
    operations: &[serde_json::Value],
    workload_result: &[serde_json::Value],
) -> Result<VerifierVerdict> {
    let workload = measurement
        .benchmark()
        .workload(&trial.workload_id)
        .with_context(|| format!("unknown workload {}", trial.workload_id))?;
    let payload = VerifierPayload {
        schema_version: 1,
        benchmark_id: measurement.benchmark_id(),
        subject_id: measurement.subject_id(),
        variant_id: measurement.variant_id(),
        workload_id: &trial.workload_id,
        engine: trial.engine,
        phase: trial.phase,
        sample_index: trial.sample_index,
        operations,
        workload_result,
    };
    match workload.verifier {
        VerifierInvocation::Exact => verify_exact(operations, workload_result),
        VerifierInvocation::Process {
            command,
            timeout,
            working_directory,
        } => verify_with_process(&payload, command, timeout, working_directory),
    }
}

fn verify_exact(
    operations: &[serde_json::Value],
    workload_result: &[serde_json::Value],
) -> Result<VerifierVerdict> {
    if operations.len() != workload_result.len() {
        return Ok(VerifierVerdict {
            success: false,
            failure_category: Some("incorrect_result".to_owned()),
            detail: Some("benchmark returned a different number of results".to_owned()),
        });
    }
    for (index, (operation, result)) in operations.iter().zip(workload_result).enumerate() {
        let expected = operation
            .as_object()
            .and_then(|operation| operation.get("expected"))
            .with_context(|| format!("exact operation {} has no expected value", index + 1))?;
        if expected != result {
            let case = operation
                .get("case_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| (index + 1).to_string());
            return Ok(VerifierVerdict {
                success: false,
                failure_category: Some("incorrect_result".to_owned()),
                detail: Some(format!("case {case} returned an unexpected result")),
            });
        }
    }
    Ok(VerifierVerdict {
        success: true,
        failure_category: None,
        detail: None,
    })
}

fn verify_with_process(
    payload: &VerifierPayload<'_>,
    command: &[String],
    timeout: Duration,
    working_directory: &Path,
) -> Result<VerifierVerdict> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("workload verifier timeout is too large for this platform")?;
    let (program, arguments) = command
        .split_first()
        .context("workload verifier has no command")?;
    let mut child = ScopedChild::new(
        Command::new(program)
            .args(arguments)
            .current_dir(working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start verifier {program:?}"))?,
    );
    let stdout = child
        .stdout
        .take()
        .context("verifier stdout was unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("verifier stderr was unavailable")?;
    let stdout_reader = thread::spawn(move || read_limited(stdout, "stdout"));
    let stderr_reader = thread::spawn(move || read_limited(stderr, "stderr"));
    let mut input = serde_json::to_vec(payload).context("failed to encode verifier payload")?;
    input.push(b'\n');
    let mut stdin = child
        .stdin
        .take()
        .context("verifier stdin was unavailable")?;
    let stdin_writer = thread::spawn(move || {
        stdin
            .write_all(&input)
            .context("failed to write verifier payload")
    });

    let status = loop {
        if let Some(status) = child.try_wait().context("failed waiting for verifier")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdin_writer.join();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!("workload verifier exceeded its {:?} timeout", timeout);
        }
        thread::sleep(Duration::from_millis(10));
    };
    stdin_writer
        .join()
        .map_err(|_| anyhow::anyhow!("verifier stdin writer panicked"))??;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("verifier stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("verifier stderr reader panicked"))??;
    if !status.success() {
        bail!(
            "workload verifier exited with {status}{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {}", String::from_utf8_lossy(&stderr))
            }
        );
    }
    let verdict: VerifierVerdict =
        serde_json::from_slice(&stdout).context("workload verifier emitted invalid JSON")?;
    match (&verdict.failure_category, verdict.success) {
        (Some(category), false) if !category.trim().is_empty() => {}
        (None, false) => bail!("failed verifier verdict has no failure_category"),
        (Some(_), true) => bail!("successful verifier verdict has a failure_category"),
        _ => {}
    }
    if verdict.success && verdict.detail.is_some() {
        bail!("successful verifier verdict has failure detail");
    }
    Ok(verdict)
}

fn read_limited(mut reader: impl Read, stream: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(MAX_VERIFIER_OUTPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read verifier {stream}"))?;
    if bytes.len() > MAX_VERIFIER_OUTPUT_BYTES {
        std::io::copy(&mut reader, &mut std::io::sink())
            .with_context(|| format!("failed to read verifier {stream}"))?;
        bail!("verifier {stream} exceeded {MAX_VERIFIER_OUTPUT_BYTES} bytes");
    }
    Ok(bytes)
}

struct ScopedChild {
    child: Child,
}

impl ScopedChild {
    fn new(child: Child) -> Self {
        Self { child }
    }
}

impl Deref for ScopedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for ScopedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for ScopedChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn make_artifact_paths_measurement_relative(
    measurement_root: &Path,
    artifact_root: &Path,
    artifacts: &mut [ArtifactEvidence],
) -> Result<()> {
    let measurement_root = fs::canonicalize(measurement_root)?;
    let artifact_root = fs::canonicalize(artifact_root)?;
    for artifact in artifacts {
        let full_path = fs::canonicalize(artifact_root.join(&artifact.path))?;
        let relative = full_path
            .strip_prefix(&measurement_root)
            .context("trial artifact escaped the measurement set")?;
        artifact.path = relative.to_string_lossy().replace('\\', "/");
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterReady {
    protocol_version: u32,
    url: String,
    #[serde(default, rename = "source_files")]
    _source_files: Vec<std::path::PathBuf>,
}

struct VariantAdapter {
    child: Child,
    url: String,
}

impl VariantAdapter {
    fn start(invocation: &VariantInvocation<'_>) -> Result<Self> {
        let (program, arguments) = invocation
            .command
            .split_first()
            .context("variant adapter has no command")?;
        let mut child = Command::new(program)
            .args(arguments)
            .current_dir(invocation.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to start variant adapter {program:?}"))?;
        let readiness = (|| -> Result<AdapterReady> {
            let stdout = child
                .stdout
                .take()
                .context("variant adapter stdout was unavailable")?;

            let (sender, receiver) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let _ = sender.send(read_adapter_readiness(stdout));
            });
            let line = match receiver.recv_timeout(invocation.ready_timeout) {
                Ok(Ok((0, _))) => {
                    let status = child.try_wait().ok().flatten();
                    bail!("variant adapter closed before readiness (status: {status:?})")
                }
                Ok(Ok((_, line))) => line,
                Ok(Err(error)) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!(
                        "variant adapter did not become ready within {:?}",
                        invocation.ready_timeout
                    )
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("variant adapter readiness reader stopped")
                }
            };
            let ready: AdapterReady = serde_json::from_str(&line)
                .context("variant adapter emitted invalid readiness JSON")?;
            if ready.protocol_version != READINESS_PROTOCOL_VERSION {
                bail!(
                    "variant readiness protocol mismatch: expected {}, received {}",
                    READINESS_PROTOCOL_VERSION,
                    ready.protocol_version
                );
            }
            if ready.url.trim().is_empty() {
                bail!("variant adapter readiness URL is empty");
            }
            Ok(ready)
        })();
        let ready = match readiness {
            Ok(readiness) => readiness,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            url: ready.url,
        })
    }

    fn url(&self) -> &str {
        &self.url
    }
}

fn read_adapter_readiness(reader: impl Read) -> Result<(usize, String)> {
    let mut line = String::new();
    let count = BufReader::new(reader.take(MAX_ADAPTER_READINESS_BYTES as u64 + 1))
        .read_line(&mut line)
        .context("failed to read variant readiness")?;
    if count > MAX_ADAPTER_READINESS_BYTES {
        bail!("variant adapter readiness exceeded {MAX_ADAPTER_READINESS_BYTES} bytes");
    }
    Ok((count, line))
}

impl Drop for VariantAdapter {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;

    use super::*;

    #[test]
    fn benchmark_host_readiness_accepts_the_reported_source_graph() {
        let ready: AdapterReady = serde_json::from_value(json!({
            "protocol_version": 2,
            "url": "http://127.0.0.1:4317/",
            "source_files": ["/benchmark.ts"]
        }))
        .unwrap();

        assert_eq!(ready.protocol_version, READINESS_PROTOCOL_VERSION);
        assert_eq!(ready.url, "http://127.0.0.1:4317/");
        assert_eq!(ready._source_files.len(), 1);
    }

    #[test]
    fn exact_verification_accepts_matching_semantic_results() {
        let verdict = verify_exact(
            &[json!({"case_id": "parser", "expected": {"value": 42}})],
            &[json!({"value": 42})],
        )
        .unwrap();

        assert!(verdict.success);
        assert_eq!(verdict.failure_category, None);
        assert_eq!(verdict.detail, None);
    }

    #[test]
    fn exact_verification_reports_the_case_that_changed() {
        let verdict = verify_exact(
            &[json!({"case_id": "parser", "expected": {"value": 42}})],
            &[json!({"value": 41})],
        )
        .unwrap();

        assert!(!verdict.success);
        assert_eq!(
            verdict.failure_category.as_deref(),
            Some("incorrect_result")
        );
        assert_eq!(
            verdict.detail.as_deref(),
            Some("case parser returned an unexpected result")
        );
    }

    #[test]
    fn exact_verification_rejects_a_malformed_operation() {
        let error = verify_exact(&[json!({"case_id": "parser"})], &[json!(null)]).unwrap_err();
        assert!(error.to_string().contains("has no expected value"));
    }

    #[test]
    fn oversized_verifier_output_is_drained_before_rejection() {
        let output = vec![b'x'; MAX_VERIFIER_OUTPUT_BYTES + 4096];
        let mut reader = std::io::Cursor::new(output);

        let error = read_limited(&mut reader, "stdout").unwrap_err();

        assert!(error.to_string().contains("exceeded"));
        assert_eq!(reader.position(), MAX_VERIFIER_OUTPUT_BYTES as u64 + 4096);
    }

    #[test]
    fn oversized_adapter_readiness_is_rejected_before_json_decode() {
        let output = vec![b'x'; MAX_ADAPTER_READINESS_BYTES + 4096];
        let mut reader = std::io::Cursor::new(output);
        let error = read_adapter_readiness(&mut reader).unwrap_err();

        assert!(
            error.to_string().contains("readiness exceeded"),
            "unexpected error: {error:#}"
        );
        assert_eq!(reader.position(), MAX_ADAPTER_READINESS_BYTES as u64 + 1);
    }

    #[test]
    fn verifier_timeout_covers_a_blocked_stdin_write() {
        let operations = [json!({"value": "x".repeat(8 * 1024 * 1024)})];
        let results = [json!(null)];
        let payload = VerifierPayload {
            schema_version: 1,
            benchmark_id: "benchmark",
            subject_id: "subject",
            variant_id: "variant",
            workload_id: "workload",
            engine: Engine::Chromium,
            phase: TrialPhase::Final,
            sample_index: 0,
            operations: &operations,
            workload_result: &results,
        };
        let command = vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--ignored".to_owned(),
            "--exact".to_owned(),
            "benchmark_runtime::tests::verifier_process_that_ignores_stdin".to_owned(),
        ];
        let started = Instant::now();
        let error = verify_with_process(
            &payload,
            &command,
            Duration::from_millis(250),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap_err();

        assert!(error.to_string().contains("exceeded its"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    #[ignore = "subprocess fixture for verifier_timeout_covers_a_blocked_stdin_write"]
    fn verifier_process_that_ignores_stdin() {
        thread::sleep(Duration::from_secs(10));
    }
}
