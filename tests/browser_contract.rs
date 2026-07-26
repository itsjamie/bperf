use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

/// This test is ignored in the fast suite because it launches three real
/// browsers and writes large diagnostic artifacts. Release/CI environments
/// that advertise browser support must run it.
#[test]
#[ignore = "requires the pinned sidecar installation and all three Playwright browsers"]
fn every_engine_satisfies_the_capture_contract() {
    let artifact_root = tempdir().expect("create conformance artifact directory");
    let output = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .args(["doctor", "--engine", "all", "--json"])
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
    assert_eq!(summary["schema_version"], 2);
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

/// A nonexistent Node executable proves the WebKit route performs capture
/// without starting Node. This remains an ignored release gate because it
/// launches the pinned WebKit browser.
#[test]
#[ignore = "requires the pinned Playwright WebKit browser"]
fn webkit_doctor_does_not_spawn_node() {
    let artifact_root = tempdir().expect("create conformance artifact directory");
    let output = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .env("BPERF_NODE", "bperf-node-must-not-be-started")
        .args(["doctor", "--engine", "webkit", "--json"])
        .arg("--artifact-dir")
        .arg(artifact_root.path())
        .output()
        .expect("run WebKit-only bperf doctor");

    assert!(
        output.status.success(),
        "doctor failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).expect("doctor emitted JSON");
    assert_eq!(summary["schema_version"], 2);
    assert_eq!(summary["engines"][0]["engine"], "webkit");
    assert_eq!(summary["engines"][0]["adapter"]["kind"], "rust-webkit");
}

/// A nonexistent Node executable proves the Chromium route performs capture
/// without starting Node.
#[test]
#[ignore = "requires the pinned Playwright Chromium browser"]
fn chromium_doctor_does_not_spawn_node() {
    let artifact_root = tempdir().expect("create conformance artifact directory");
    let output = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .env("BPERF_NODE", "bperf-node-must-not-be-started")
        .args(["doctor", "--engine", "chromium", "--json"])
        .arg("--artifact-dir")
        .arg(artifact_root.path())
        .output()
        .expect("run Chromium-only bperf doctor");

    assert!(
        output.status.success(),
        "doctor failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).expect("doctor emitted JSON");
    assert_eq!(summary["schema_version"], 2);
    assert_eq!(summary["engines"][0]["engine"], "chromium");
    assert_eq!(summary["engines"][0]["adapter"]["kind"], "rust-chromium");
}

/// A nonexistent Node executable proves the Firefox route is owned entirely by
/// the Rust browser laboratory.
#[test]
#[ignore = "requires the pinned Playwright Firefox browser"]
fn firefox_doctor_does_not_spawn_node() {
    let artifact_root = tempdir().expect("create conformance artifact directory");
    let output = Command::new(env!("CARGO_BIN_EXE_bperf"))
        .env("BPERF_NODE", "bperf-node-must-not-be-started")
        .args(["doctor", "--engine", "firefox", "--json"])
        .arg("--artifact-dir")
        .arg(artifact_root.path())
        .output()
        .expect("run Firefox-only bperf doctor");

    assert!(
        output.status.success(),
        "doctor failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).expect("doctor emitted JSON");
    assert_eq!(summary["schema_version"], 2);
    assert_eq!(summary["engines"][0]["engine"], "firefox");
    assert_eq!(summary["engines"][0]["adapter"]["kind"], "rust-firefox");
}
