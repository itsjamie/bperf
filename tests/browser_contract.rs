use std::{env, process::Command};

use serde_json::Value;
use tempfile::tempdir;

/// This test is ignored in the fast suite because it launches three real
/// browsers and writes large diagnostic artifacts. Release/CI environments
/// that advertise browser support must run it.
#[test]
#[ignore = "requires the pinned Node sidecar and all three Playwright browsers"]
fn every_engine_satisfies_the_capture_contract() {
    let node = env::var_os("BPERF_NODE")
        .expect("set BPERF_NODE to the Node executable used by the sidecar");
    let artifact_root = tempdir().expect("create conformance artifact directory");
    let output = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .args(["doctor", "--engine", "all", "--json", "--node"])
        .arg(node)
        .arg("--artifact-dir")
        .arg(artifact_root.path())
        .output()
        .expect("run bperf doctor");

    assert!(
        output.status.success(),
        "doctor failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let summary: Value = serde_json::from_slice(&output.stdout).expect("doctor emitted JSON");
    assert_eq!(summary["verdict"], "supported");
    let engines = summary["engines"].as_array().expect("engines is an array");
    assert_eq!(engines.len(), 3);
    assert_eq!(
        engines
            .iter()
            .map(|engine| engine["engine"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["chromium", "firefox", "webkit"]
    );

    for engine in engines {
        for capability in [
            "isolated_launch",
            "process_root",
            "cpu_profile",
            "js_heap",
            "flamegraph",
        ] {
            assert_eq!(
                engine["capabilities"][capability], true,
                "{} did not satisfy {capability}",
                engine["engine"]
            );
        }
        assert_eq!(engine["artifacts"].as_array().unwrap().len(), 3);
    }
}
