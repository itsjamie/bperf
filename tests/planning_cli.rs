use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use bperf_storage::database::Database;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn bperf() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bperf"))
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

fn plan(benchmark: &Path, variant: &Path, artifact_root: &Path) -> (Value, PathBuf) {
    let output = bperf()
        .args(["plan", "--final-samples", "20", "--artifact-dir"])
        .arg(artifact_root)
        .arg("--json")
        .arg(benchmark)
        .arg(variant)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    let root = PathBuf::from(summary["measurement_root"].as_str().unwrap());
    (summary, root)
}

fn complete_measurement(root: &Path, value: f64, environment: &str) {
    let measurement_set_id = root.file_name().unwrap().to_str().unwrap();
    let database = Database::for_collection(root.parent().unwrap(), "measurements").unwrap();
    let os_release = format!("test-{environment}");
    let identity_source = format!(
        concat!(
            "{{",
            "\"bperf_version\":\"0.1.0\",",
            "\"browser_lab_protocol_version\":13,",
            "\"host\":{{",
            "\"platform\":\"windows\",",
            "\"arch\":\"x64\",",
            "\"os_release\":{},",
            "\"cpu_model\":\"test\",",
            "\"logical_cpus\":8,",
            "\"total_memory_bytes\":16000000000",
            "}},",
            "\"adapters\":{{",
            "\"chromium\":{{",
            "\"kind\":\"rust-chromium\",",
            "\"playwright\":\"1.61.1\",",
            "\"chromium_revision\":\"1228\",",
            "\"executable_sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
            "\"protocol_version\":2,",
            "\"browser_workload_version\":1",
            "}},",
            "\"firefox\":{{",
            "\"kind\":\"rust-firefox\",",
            "\"playwright\":\"1.61.1\",",
            "\"firefox_revision\":\"1532\",",
            "\"executable_sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",",
            "\"protocol_version\":2,",
            "\"browser_workload_version\":1",
            "}},",
            "\"webkit\":{{",
            "\"kind\":\"rust-webkit\",",
            "\"playwright\":\"1.61.1\",",
            "\"webkit_revision\":\"test\",",
            "\"executable_sha256\":\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\",",
            "\"protocol_version\":2,",
            "\"browser_workload_version\":1",
            "}}",
            "}},",
            "\"browsers\":{{",
            "\"chromium\":{{\"executable_path\":\"chromium\",\"version\":\"1\"}},",
            "\"firefox\":{{\"executable_path\":\"firefox\",\"version\":\"1\"}},",
            "\"webkit\":{{\"executable_path\":\"webkit\",\"version\":\"1\"}}",
            "}}",
            "}}"
        ),
        serde_json::to_string(&os_release).unwrap()
    );
    let mut environment_digest = Sha256::new();
    environment_digest.update(b"bperf-browser-environment-v4\0");
    environment_digest.update(identity_source.as_bytes());
    let environment_fingerprint = format!("{:x}", environment_digest.finalize());
    let identity: Value = serde_json::from_str(&identity_source).unwrap();
    let anchors = serde_json::json!({
        "chromium": {
            "workload": "javascript_cpu_v1",
            "wall_ms": vec![10.0; 31],
            "batch_size": 1,
            "checksum": 42
        },
        "firefox": {
            "workload": "javascript_cpu_v1",
            "wall_ms": vec![10.0; 31],
            "batch_size": 1,
            "checksum": 42
        },
        "webkit": {
            "workload": "javascript_cpu_v1",
            "wall_ms": vec![10.0; 31],
            "batch_size": 1,
            "checksum": 42
        }
    });
    database
        .publish_document(
            "measurement",
            &format!("{measurement_set_id}/environment"),
            &serde_json::json!({
            "schema_version": 7,
            "recorded_at_unix_ms": 1,
            "fingerprint": environment_fingerprint,
            "identity": identity,
            "anchors": anchors
            }),
        )
        .unwrap();

    let schedule: Value = database
        .read_document("measurement", &format!("{measurement_set_id}/schedule"))
        .unwrap()
        .unwrap();
    for trial in schedule["trials"].as_array().unwrap() {
        let trial_id = trial["trial_id"].as_str().unwrap();
        let artifact_dir = root.join("synthetic-artifacts").join(trial_id);
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut artifacts = Vec::new();
        let mut artifact_kinds: Vec<(&str, &str)> = vec![
            ("cpu_profile", "cpu.json"),
            ("js_heap", "heap.json"),
            ("flamegraph", "flamegraph.json"),
        ];
        if trial["engine"] == "chromium" {
            artifact_kinds.push(("heap_sampling_profile", "heap-sampling.json"));
        }
        for (kind, name) in artifact_kinds {
            let bytes = format!("{trial_id}-{kind}").into_bytes();
            let path = artifact_dir.join(name);
            fs::write(&path, &bytes).unwrap();
            artifacts.push(serde_json::json!({
                "capture_scope": if trial["engine"] == "firefox" {
                    "browser-context"
                } else {
                    "page"
                },
                "kind": kind,
                "path": path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                "size_bytes": bytes.len(),
                "sha256": format!("{:x}", Sha256::digest(&bytes)),
                "format": "synthetic test evidence"
            }));
        }
        // WebKit has no `Heap.getStatistics` in the pinned protocol yet
        // (bperf issue #4), so its synthetic trials mark the allocation
        // metric unsupported instead of measuring it.
        let (allocated_metric, unsupported_metrics) = if trial["engine"] == "webkit" {
            (
                serde_json::Map::new(),
                serde_json::json!({
                    "browser.js_heap.allocated_bytes":
                        "pinned WebKit protocol has no Heap.getStatistics"
                }),
            )
        } else {
            let mut metric = serde_json::Map::new();
            metric.insert("browser.js_heap.allocated_bytes".to_owned(), value.into());
            (metric, serde_json::json!({}))
        };
        let mut metrics = serde_json::json!({
            "workload.wall_ms": value,
            "variant.call_wall_ms": value,
            "browser.cpu_profile.active_ms": value,
            "browser.js_heap.live_bytes": value,
            "bperf.capture.elapsed_ms": value,
            "bperf.batch_size": 1,
            "bperf.trial.elapsed_ms": value
        });
        metrics.as_object_mut().unwrap().extend(allocated_metric);
        database
            .append_event(
                "measurement_trials",
                measurement_set_id,
                &serde_json::json!({
                "schema_version": 7,
                "measurement_set_id": measurement_set_id,
                "trial_id": trial["trial_id"],
                "attempt": 1,
                "workload_id": trial["workload_id"],
                "engine": trial["engine"],
                "phase": trial["phase"],
                "sample_index": trial["sample_index"],
                "environment_fingerprint": environment_fingerprint,
                "valid": true,
                "success": true,
                "metrics": metrics,
                "unsupported_metrics": unsupported_metrics,
                "artifacts": artifacts
                }),
            )
            .unwrap();
    }
}

fn write_lineage_fixture(
    root: &Path,
    baseline_root: &Path,
    candidate_root: &Path,
    comparison: &Value,
) -> String {
    let objects = root.join("objects");
    fs::create_dir_all(&objects).unwrap();
    let database = Database::for_collection(root, "lineages").unwrap();

    let before = b"export const value = 1;\n";
    let after = b"export const value = 2;\n";
    let before_hash = format!("{:x}", Sha256::digest(before));
    let after_hash = format!("{:x}", Sha256::digest(after));
    fs::write(objects.join(&before_hash), before).unwrap();
    fs::write(objects.join(&after_hash), after).unwrap();

    let first_state = format!("state-{}", "1".repeat(64));
    let second_state = format!("state-{}", "2".repeat(64));
    let first_change = format!("change-{}", "1".repeat(64));
    let second_change = format!("change-{}", "2".repeat(64));
    for (state_id, digest, size) in [
        (&first_state, &before_hash, before.len()),
        (&second_state, &after_hash, after.len()),
    ] {
        database
            .publish_document(
                "source_state",
                state_id,
                &serde_json::json!({
                    "schema_version": 1,
                    "state_id": state_id,
                    "files": [{
                        "path": "src/implementation.js",
                        "sha256": digest,
                        "size_bytes": size
                    }]
                }),
            )
            .unwrap();
    }
    database
        .publish_document(
            "source_change",
            &first_change,
            &serde_json::json!({
                "schema_version": 1,
                "change_id": first_change,
                "source_before": null,
                "source_after": first_state,
                "files": [{
                    "path": "src/implementation.js",
                    "kind": "added",
                    "before_sha256": null,
                    "after_sha256": before_hash
                }]
            }),
        )
        .unwrap();
    database
        .publish_document(
            "source_change",
            &second_change,
            &serde_json::json!({
                "schema_version": 1,
                "change_id": second_change,
                "source_before": first_state,
                "source_after": second_state,
                "files": [{
                    "path": "src/implementation.js",
                    "kind": "modified",
                    "before_sha256": before_hash,
                    "after_sha256": after_hash
                }]
            }),
        )
        .unwrap();

    let engine_summaries: Vec<_> = comparison["engines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|engine| {
            let metrics = engine["effects"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, effect)| {
                    (
                        name.clone(),
                        serde_json::json!({
                            "improvement_pct": effect["improvement_pct"],
                            "ci_pct": effect["ci_pct"],
                            "classification": effect["classification"],
                            "guardrail_regressed": effect["guardrail_regressed"]
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            serde_json::json!({
                "engine": engine["engine"],
                "verdict": engine["verdict"],
                "correctness": engine["correctness"]["gate"],
                "anchor": {
                    "status": engine["anchor"]["status"],
                    "drift_pct": engine["anchor"]["drift_pct"],
                    "ci_pct": engine["anchor"]["ci_pct"]
                },
                "metrics": metrics
            })
        })
        .collect();
    let first_cycle = format!("cycle-{}", "1".repeat(64));
    let second_cycle = format!("cycle-{}", "2".repeat(64));
    let first = serde_json::json!({
        "event": "cycle",
        "record": {
            "schema_version": 1,
            "cycle_id": first_cycle,
            "previous_cycle_id": null,
            "recorded_at_unix_ms": 1,
            "benchmark_id": comparison["benchmark_id"],
            "subject_id": comparison["subject_id"],
            "benchmark_sha256": comparison["benchmark_sha256"],
            "message": "establish baseline",
            "source_before": null,
            "source_after": first_state,
            "change_id": first_change,
            "baseline_measurement_set": null,
            "candidate_measurement_set": comparison["baseline"]["measurement_set_id"],
            "candidate_measurement_path": baseline_root.to_string_lossy(),
            "environment_fingerprint": comparison["environment_fingerprint"],
            "outcome": "measured",
            "comparison": null
        }
    });
    let second = serde_json::json!({
        "event": "cycle",
        "record": {
            "schema_version": 1,
            "cycle_id": second_cycle,
            "previous_cycle_id": first_cycle,
            "recorded_at_unix_ms": 2,
            "benchmark_id": comparison["benchmark_id"],
            "subject_id": comparison["subject_id"],
            "benchmark_sha256": comparison["benchmark_sha256"],
            "message": "reuse parsed boxes",
            "source_before": first_state,
            "source_after": second_state,
            "change_id": second_change,
            "baseline_measurement_set": comparison["baseline"]["measurement_set_id"],
            "candidate_measurement_set": comparison["candidate"]["measurement_set_id"],
            "candidate_measurement_path": candidate_root.to_string_lossy(),
            "environment_fingerprint": comparison["environment_fingerprint"],
            "outcome": comparison["verdict"],
            "comparison": {
                "comparison_id": comparison["comparison_id"],
                "baseline_measurement_set": comparison["baseline"]["measurement_set_id"],
                "candidate_measurement_set": comparison["candidate"]["measurement_set_id"],
                "environment_fingerprint": comparison["environment_fingerprint"],
                "policy": "strict_all",
                "verdict": comparison["verdict"],
                "engines": engine_summaries,
                "warnings": comparison["warnings"]
            }
        }
    });
    database
        .append_event("lineage", "browser-operation-benchmark", &first)
        .unwrap();
    database
        .append_event("lineage", "browser-operation-benchmark", &second)
        .unwrap();

    let environment = |recorded_at_unix_ms| {
        serde_json::json!({
            "recorded_at_unix_ms": recorded_at_unix_ms,
            "fingerprint": comparison["environment_fingerprint"],
            "platform": "windows",
            "arch": "x86_64",
            "os_release": "test",
            "browser_versions": {
                "chromium": "1",
                "firefox": "1",
                "webkit": "1"
            }
        })
    };
    for (cycle_id, variant_id, recorded_at_unix_ms, additions, deletions) in [
        (&first_cycle, &comparison["baseline"]["variant_id"], 1, 1, 0),
        (
            &second_cycle,
            &comparison["candidate"]["variant_id"],
            2,
            1,
            1,
        ),
    ] {
        database
            .publish_document(
                "history_evidence",
                cycle_id,
                &serde_json::json!({
                    "schema_version": 1,
                    "cycle_id": cycle_id,
                    "variant_id": variant_id,
                    "case_ids": ["checkout-flow"],
                    "environment": environment(recorded_at_unix_ms),
                    "change": {
                        "files_changed": 1,
                        "additions": additions,
                        "deletions": deletions,
                        "binary_files": 0
                    },
                    "artifacts": []
                }),
            )
            .unwrap();
    }
    second_cycle
}

#[test]
fn independent_measurements_can_be_promoted_and_compared() {
    let benchmark = example("browser-benchmark.yaml");
    let baseline_variant = example("browser-variant-baseline.yaml");
    let candidate_variant = example("browser-variant-candidate.yaml");

    let validation = bperf()
        .args(["validate", "--json", "--variant"])
        .arg(&baseline_variant)
        .arg(&benchmark)
        .output()
        .unwrap();
    assert!(
        validation.status.success(),
        "{}",
        String::from_utf8_lossy(&validation.stderr)
    );
    let validation: Value = serde_json::from_slice(&validation.stdout).unwrap();
    assert_eq!(validation["status"], "valid");
    assert_eq!(validation["benchmark_id"], "browser-operation-benchmark");
    assert_eq!(validation["subject_id"], "browser-operation-adapter");
    assert_eq!(validation["variant"]["id"], "browser-operation-main");
    assert_eq!(
        validation["engines"],
        serde_json::json!(["chromium", "firefox", "webkit"])
    );

    let directory = tempdir().unwrap();
    let measurements = directory.path().join("measurements");
    let (baseline_plan, baseline_root) = plan(&benchmark, &baseline_variant, &measurements);
    let (candidate_plan, candidate_root) = plan(&benchmark, &candidate_variant, &measurements);
    assert_eq!(baseline_plan["trial_count"], 99);
    assert_eq!(baseline_plan["final_trial_count"], 60);
    assert_ne!(
        baseline_plan["measurement_set_id"],
        candidate_plan["measurement_set_id"]
    );
    for root in [&baseline_root, &candidate_root] {
        assert!(!root.join("benchmark.resolved.json").exists());
        assert!(!root.join("variant.resolved.json").exists());
        assert!(!root.join("schedule.json").exists());
    }
    assert!(directory.path().join("bperf.sqlite3").is_file());

    let registry = directory.path().join("baselines");
    let incomplete_promotion = bperf()
        .args(["baseline", "promote"])
        .arg(&baseline_root)
        .arg("--registry-dir")
        .arg(&registry)
        .output()
        .unwrap();
    assert!(!incomplete_promotion.status.success());
    assert!(String::from_utf8_lossy(&incomplete_promotion.stderr).contains("is incomplete"));

    complete_measurement(&baseline_root, 100.0, "pinned-environment");
    complete_measurement(&candidate_root, 90.0, "pinned-environment");

    let promotion = bperf()
        .args(["baseline", "promote"])
        .arg(&baseline_root)
        .arg("--registry-dir")
        .arg(&registry)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        promotion.status.success(),
        "{}",
        String::from_utf8_lossy(&promotion.stderr)
    );
    let promotion: Value = serde_json::from_slice(&promotion.stdout).unwrap();
    assert_eq!(
        promotion["measurement_set_id"],
        baseline_plan["measurement_set_id"]
    );
    let repeated_promotion = bperf()
        .args(["baseline", "promote"])
        .arg(&baseline_root)
        .arg("--registry-dir")
        .arg(&registry)
        .output()
        .unwrap();
    assert!(repeated_promotion.status.success());
    let database = Database::open(directory.path()).unwrap();
    assert_eq!(
        database
            .read_events::<Value>("baseline", "browser-operation-benchmark")
            .unwrap()
            .len(),
        1
    );

    let comparisons = directory.path().join("comparisons");
    let comparison = bperf()
        .arg("compare")
        .arg(&candidate_root)
        .arg("--registry-dir")
        .arg(&registry)
        .arg("--artifact-dir")
        .arg(&comparisons)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        comparison.status.success(),
        "{}",
        String::from_utf8_lossy(&comparison.stderr)
    );
    let comparison: Value = serde_json::from_slice(&comparison.stdout).unwrap();
    assert_eq!(comparison["verdict"], "positive");
    assert_eq!(
        comparison["method"],
        "independent_two_sample_hierarchical_bootstrap"
    );
    assert_eq!(
        comparison["baseline"]["measurement_set_id"],
        baseline_plan["measurement_set_id"]
    );
    assert_eq!(
        comparison["candidate"]["measurement_set_id"],
        candidate_plan["measurement_set_id"]
    );
    assert!(
        database
            .read_document_bytes("comparison", comparison["comparison_id"].as_str().unwrap(),)
            .unwrap()
            .is_some()
    );
    assert!(
        !comparisons.exists(),
        "default compare must not materialize JSON"
    );
    let compact_comparison = bperf()
        .arg("compare")
        .arg(&candidate_root)
        .arg("--registry-dir")
        .arg(&registry)
        .arg("--artifact-dir")
        .arg(&comparisons)
        .output()
        .unwrap();
    assert!(compact_comparison.status.success());
    let compact_comparison = String::from_utf8(compact_comparison.stdout).unwrap();
    for expected in [
        "bperf compare: positive",
        "chromium: positive correctness=pass anchor=stable",
        "firefox: positive correctness=pass anchor=stable",
        "webkit: positive correctness=pass anchor=stable",
        "workload.wall_ms: improved effect=+10.00% (100ms -> 90ms) ci=[+10.00%, +10.00%]",
        "browser.js_heap.allocated_bytes: improved effect=+10.00% (100b -> 90b) ci=[+10.00%, +10.00%]",
        "browser.js_heap.allocated_bytes: n/a (unsupported: pinned WebKit protocol has no Heap.getStatistics)",
        comparison["comparison_id"].as_str().unwrap(),
    ] {
        assert!(
            compact_comparison.contains(expected),
            "compact comparison omitted {expected:?}:\n{compact_comparison}"
        );
    }

    let lineages = directory.path().join("lineages");
    fs::create_dir_all(&lineages).unwrap();
    let cycle_id = write_lineage_fixture(&lineages, &baseline_root, &candidate_root, &comparison);
    let simple_history = bperf()
        .arg("history")
        .arg("--lineage-dir")
        .arg(&lineages)
        .output()
        .unwrap();
    assert!(
        simple_history.status.success(),
        "{}",
        String::from_utf8_lossy(&simple_history.stderr)
    );
    let simple_history = String::from_utf8(simple_history.stdout).unwrap();
    for expected in [
        "bperf history: browser-operation-benchmark (2 runs)",
        "case: checkout-flow",
        "cycle",
        "message",
        "cycle-11111111111111",
        "establish baseline",
        "cycle-22222222222222",
        "reuse parsed boxes",
    ] {
        assert!(
            simple_history.contains(expected),
            "simple history omitted {expected:?}:\n{simple_history}"
        );
    }
    for internal_detail in [
        "measurement set:",
        "source:",
        "chromium",
        "100ms",
        "positive",
        &cycle_id,
    ] {
        assert!(
            !simple_history.contains(internal_detail),
            "simple history exposed {internal_detail:?}:\n{simple_history}"
        );
    }

    let history = bperf()
        .args([
            "history",
            "browser-operation-benchmark",
            "--format",
            "agent-context",
            "--lineage-dir",
        ])
        .arg(&lineages)
        .output()
        .unwrap();
    assert!(history.status.success());
    let history = String::from_utf8(history.stdout).unwrap();
    for expected in [
        "reuse parsed boxes",
        "chromium",
        "firefox",
        "webkit",
        "anchor=stable",
        "+export const value = 2;",
    ] {
        assert!(history.contains(expected), "history omitted {expected:?}");
    }

    let shown = bperf()
        .arg("show")
        .arg(&cycle_id)
        .arg("--diff")
        .arg("--json")
        .arg("--lineage-dir")
        .arg(&lineages)
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["cycle"]["cycle_id"], cycle_id);
    assert!(
        shown["diff"]
            .as_str()
            .unwrap()
            .contains("-export const value = 1;")
    );

    for _ in 0..2 {
        let accepted = bperf()
            .arg("accept")
            .arg(&cycle_id)
            .arg("--lineage-dir")
            .arg(&lineages)
            .arg("--registry-dir")
            .arg(&registry)
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            accepted.status.success(),
            "{}",
            String::from_utf8_lossy(&accepted.stderr)
        );
        let accepted: Value = serde_json::from_slice(&accepted.stdout).unwrap();
        assert_eq!(accepted["status"], "accepted");
        assert_eq!(
            accepted["baseline"]["measurement_set_id"],
            candidate_plan["measurement_set_id"]
        );
    }
    assert_eq!(
        database
            .read_events::<Value>("lineage", "browser-operation-benchmark")
            .unwrap()
            .len(),
        3,
        "repeated acceptance must not duplicate a promotion event"
    );
    assert!(!lineages.join("browser-operation-benchmark.jsonl").exists());
    assert_eq!(
        database
            .read_events::<Value>("baseline", "browser-operation-benchmark")
            .unwrap()
            .len(),
        2,
        "repeated acceptance must not duplicate a baseline reference"
    );
    let accepted_history = bperf()
        .arg("history")
        .arg("--lineage-dir")
        .arg(&lineages)
        .output()
        .unwrap();
    assert!(accepted_history.status.success());
    let accepted_history = String::from_utf8(accepted_history.stdout).unwrap();
    assert!(accepted_history.contains("reuse parsed boxes [baseline]"));
    assert!(
        !accepted_history.contains("promotion "),
        "plain history should mark the current baseline without listing storage events:\n{accepted_history}"
    );
    let interactive_snapshot = bperf_decision::lineage::history_view(&lineages, None).unwrap();
    assert_eq!(
        interactive_snapshot.benchmark_id,
        "browser-operation-benchmark"
    );
    assert_eq!(interactive_snapshot.cycles.len(), 2);
    assert_eq!(
        interactive_snapshot.current_baseline_label.as_deref(),
        Some("b-02")
    );
    let accepted_cycle = interactive_snapshot
        .cycles
        .iter()
        .find(|cycle| cycle.accepted)
        .unwrap();
    assert!(accepted_cycle.current_baseline);
    assert_eq!(accepted_cycle.accepted_label.as_deref(), Some("b-02"));
    let candidate_cycle = interactive_snapshot
        .cycles
        .iter()
        .find(|cycle| cycle.outcome == "positive")
        .unwrap();
    assert_eq!(candidate_cycle.change.files_changed, 1);
    assert_eq!(candidate_cycle.change.additions, 1);
    assert_eq!(candidate_cycle.change.deletions, 1);
    assert_eq!(candidate_cycle.baseline_label.as_deref(), Some("b-01"));
    assert!(candidate_cycle.artifacts.iter().all(|artifact| !matches!(
        artifact.kind,
        bperf_decision::lineage::HistoryArtifactKind::Comparison
            | bperf_decision::lineage::HistoryArtifactKind::Sampling
    )));

    let overview = bperf_decision::lineage::history_overview(&lineages, None).unwrap();
    assert_eq!(overview.cycles.len(), interactive_snapshot.cycles.len());
    let candidate_summary = overview
        .cycles
        .iter()
        .find(|cycle| cycle.outcome == "positive")
        .unwrap();
    let selected_cycle = bperf_decision::lineage::history_cycle(
        &lineages,
        &overview.benchmark_id,
        &candidate_summary.cycle_id,
    )
    .unwrap();
    assert_eq!(selected_cycle.cycle_id, candidate_cycle.cycle_id);
    assert_eq!(selected_cycle.change.files_changed, 1);
    assert_eq!(
        selected_cycle.artifacts.len(),
        candidate_cycle.artifacts.len()
    );

    let redirected_bare_history = bperf()
        .arg("--data-dir")
        .arg(directory.path())
        .arg("history")
        .output()
        .unwrap();
    assert!(redirected_bare_history.status.success());
    assert!(
        String::from_utf8(redirected_bare_history.stdout)
            .unwrap()
            .contains("bperf history: browser-operation-benchmark"),
        "bare history must retain text output when stdout is not a terminal"
    );

    let different_measurements = directory.path().join("different").join("measurements");
    let (_, different_candidate_root) =
        plan(&benchmark, &candidate_variant, &different_measurements);
    complete_measurement(&different_candidate_root, 90.0, "different-environment");
    let incompatible = bperf()
        .arg("compare")
        .arg(&different_candidate_root)
        .arg("--baseline")
        .arg(&baseline_root)
        .arg("--artifact-dir")
        .arg(&comparisons)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!incompatible.status.success());
    assert!(
        String::from_utf8_lossy(&incompatible.stderr)
            .contains("different pinned browser/runtime identities")
    );
}
