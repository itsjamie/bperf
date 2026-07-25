use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;
use tempfile::tempdir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn measure(benchmark: &Path, variant: &Path, root: &Path, node: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .arg("measure")
        .arg(benchmark)
        .arg(variant)
        .arg("--artifact-dir")
        .arg(root)
        .arg("--node")
        .arg(node)
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
#[ignore = "requires the pinned Node sidecar and all three Playwright browsers"]
fn variants_can_be_measured_and_compared_on_every_engine() {
    let node = PathBuf::from(
        env::var_os("BPERF_NODE")
            .expect("set BPERF_NODE to the Node executable used by the sidecar"),
    );
    let directory = tempdir().expect("create measurement artifact directory");
    let benchmark = fixture("browser-smoke.yaml");
    let baseline = measure(
        &benchmark,
        &fixture("variant-baseline.yaml"),
        directory.path(),
        &node,
    );
    let candidate = measure(
        &benchmark,
        &fixture("variant-candidate.yaml"),
        directory.path(),
        &node,
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
        let trials =
            fs::read_to_string(measurement_root.join("trials.jsonl")).expect("read trial log");
        let records: Vec<Value> = trials
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid trial JSON"))
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records
                .iter()
                .map(|record| record["engine"].as_str().unwrap())
                .collect::<HashSet<_>>(),
            HashSet::from(["chromium", "firefox", "webkit"])
        );
        for record in records {
            assert_eq!(record["valid"], true);
            assert_eq!(record["success"], true);
            assert_eq!(record["artifacts"].as_array().unwrap().len(), 3);
            for metric in [
                "workload.wall_ms",
                "browser.cpu_profile.active_ms",
                "browser.js_heap.live_bytes",
            ] {
                assert!(record["metrics"][metric].as_f64().unwrap() > 0.0);
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
    assert!(
        comparison.status.success(),
        "compare failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&comparison.stdout),
        String::from_utf8_lossy(&comparison.stderr)
    );
    let comparison: Value =
        serde_json::from_slice(&comparison.stdout).expect("comparison emitted JSON");
    assert_eq!(comparison["verdict"], "positive");
    assert_eq!(comparison["engines"].as_array().unwrap().len(), 3);
}
