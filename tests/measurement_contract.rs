use std::{
    path::{Path, PathBuf},
    process::Command,
};

use bperf_browser::lab::Engine;
use bperf_measurement::store::MeasurementSet;
use serde_json::Value;
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn measure(benchmark: &Path, variant: &Path, root: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .arg("measure")
        .arg(benchmark)
        .arg(variant)
        .arg("--artifact-dir")
        .arg(root)
        .arg("--json")
        .output()
        .expect("run bperf measure");
    assert!(
        output.status.success(),
        "measure failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("measure emitted JSON")
}

/// The ignored contract test is the release gate for the complete browser
/// matrix; unit tests use protocol fixtures and never substitute one engine for
/// another.
#[test]
#[ignore = "requires the pinned runtime and all three Playwright browsers"]
fn variants_can_be_measured_and_compared_on_every_engine() {
    let directory = tempdir().expect("create measurement artifact directory");
    let benchmark = fixture("browser-smoke.yaml");
    let baseline = measure(
        &benchmark,
        &fixture("variant-baseline.yaml"),
        directory.path(),
    );
    let candidate = measure(
        &benchmark,
        &fixture("variant-candidate.yaml"),
        directory.path(),
    );

    for summary in [&baseline, &candidate] {
        assert_eq!(summary["status"], "complete");
        assert_eq!(summary["completed_trials"], 3);
        assert_eq!(summary["final_complete"], true);
        assert_eq!(summary["artifact_retention"]["retained_artifacts"], 9);
        assert_eq!(summary["artifact_retention"]["discarded_artifacts"], 0);
        let measurement_root =
            PathBuf::from(summary["measurement_root"].as_str().expect("root path"));
        assert!(!measurement_root.join("preflight").exists());
        assert!(!measurement_root.join("workloads").exists());
        let measurement =
            MeasurementSet::open(&measurement_root).expect("open completed measurement set");
        assert_eq!(measurement.completed_active_trial_count(), 3);
        for engine in Engine::ALL {
            let records = measurement.final_results(engine);
            assert_eq!(records.len(), 1, "expected one final {engine} trial");
            let record = records[0];
            assert_eq!(record.engine, engine);
            assert!(record.valid);
            assert!(record.success);
            assert_eq!(record.artifacts.len(), 3);
            for metric in [
                "workload.wall_ms",
                "browser.cpu_profile.active_ms",
                "browser.js_heap.live_bytes",
            ] {
                assert!(record.metrics.get(metric).copied().unwrap() > 0.0);
            }
            // WebKit has no `Heap.getStatistics` in the pinned protocol yet
            // (bperf issue #4); every other engine measures the metric.
            if engine == Engine::Webkit {
                assert!(
                    !record
                        .metrics
                        .contains_key("browser.js_heap.allocated_bytes")
                );
                assert!(!record.unsupported_metrics["browser.js_heap.allocated_bytes"].is_empty());
            } else {
                assert!(
                    record.metrics["browser.js_heap.allocated_bytes"] > 0.0,
                    "expected {engine} to measure allocated bytes"
                );
                assert!(
                    !record
                        .unsupported_metrics
                        .contains_key("browser.js_heap.allocated_bytes")
                );
            }
        }
    }

    let comparison = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .arg("compare")
        .arg(candidate["measurement_root"].as_str().unwrap())
        .arg("--baseline")
        .arg(baseline["measurement_root"].as_str().unwrap())
        .arg("--artifact-dir")
        .arg(directory.path().join("comparisons"))
        .arg("--json")
        .output()
        .expect("run bperf compare");
    let comparison_exit_code = comparison.status.code();
    // A noisy host may legitimately leave the fresh runtime anchor
    // inconclusive; the assertions below still reject drift, failed
    // correctness, incomplete engine coverage, and non-improving effects.
    assert!(
        matches!(comparison_exit_code, Some(0 | 2)),
        "compare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&comparison.stdout),
        String::from_utf8_lossy(&comparison.stderr)
    );
    let comparison: Value =
        serde_json::from_slice(&comparison.stdout).expect("comparison emitted JSON");
    let verdict = comparison["verdict"].as_str().expect("comparison verdict");
    assert!(
        matches!(verdict, "positive" | "inconclusive"),
        "unexpected comparison verdict: {verdict}"
    );
    assert_eq!(
        comparison_exit_code,
        Some(if verdict == "positive" { 0 } else { 2 })
    );

    let engines = comparison["engines"].as_array().expect("engine reports");
    assert_eq!(engines.len(), 3);
    for engine in engines {
        assert_eq!(engine["correctness"]["gate"], "pass");
        for effect in engine["effects"]
            .as_object()
            .expect("metric effects")
            .values()
        {
            assert_eq!(effect["classification"], "improved");
        }
    }
    if verdict == "positive" {
        assert!(engines.iter().all(|engine| engine["verdict"] == "positive"));
    } else {
        assert_eq!(comparison["stability"]["status"], "inconclusive");
        assert!(engines.iter().any(|engine| {
            engine["anchor"]["status"] == "inconclusive" && engine["verdict"] == "inconclusive"
        }));
        assert!(engines.iter().all(|engine| {
            matches!(
                engine["anchor"]["status"].as_str(),
                Some("stable" | "inconclusive")
            )
        }));
    }
}
