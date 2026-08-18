use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;
use tempfile::tempdir;

fn bperf() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bperf"))
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

#[test]
fn help_keeps_the_common_workflow_small() {
    let top_level = bperf().arg("--help").output().unwrap();
    assert!(top_level.status.success());
    let top_level = String::from_utf8(top_level.stdout).unwrap();
    for expected in [
        "Measure browser code and decide whether a change helped",
        "bperf run benchmarks/parser.bench.ts",
        "bperf accept",
        "--data-dir <DIR>",
        "Advanced integration commands: validate, plan, measure, compare, baseline.",
    ] {
        assert!(
            top_level.contains(expected),
            "top-level help omitted {expected:?}:\n{top_level}"
        );
    }

    let run = bperf().args(["run", "--help"]).output().unwrap();
    assert!(run.status.success());
    let run = String::from_utf8(run.stdout).unwrap();
    for expected in [
        "TypeScript benchmark module or directory",
        "Omit it to browse ./benchmarks",
        "-b, --budget",
        "-m, --message",
        "-j, --json",
    ] {
        assert!(
            run.contains(expected),
            "run help omitted {expected:?}:\n{run}"
        );
    }
    for leaked_storage_detail in [
        "--artifact-dir",
        "--state-dir",
        "--object-dir",
        "--registry-dir",
        "--comparison-dir",
        "--lineage-dir",
    ] {
        assert!(
            !run.contains(leaked_storage_detail),
            "run help exposed {leaked_storage_detail:?}:\n{run}"
        );
    }
}

#[test]
fn interactive_run_selection_requires_a_terminal() {
    let bare = bperf().arg("run").output().unwrap();
    assert!(!bare.status.success());
    let stderr = String::from_utf8_lossy(&bare.stderr);
    assert!(
        stderr.contains("interactive benchmark selection requires a terminal"),
        "bare run stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("pass a .bench.ts file"),
        "bare run stderr:\n{stderr}"
    );

    let temporary = tempdir().unwrap();
    let directory = temporary.path().join("benchmarks");
    fs::create_dir(&directory).unwrap();
    let directory_run = bperf().arg("run").arg(&directory).output().unwrap();
    assert!(!directory_run.status.success());
    let stderr = String::from_utf8_lossy(&directory_run.stderr);
    assert!(
        stderr.contains("interactive benchmark selection requires a terminal"),
        "directory run stderr:\n{stderr}"
    );
}

#[test]
fn cycle_commands_default_to_the_latest_work() {
    for command in ["show", "accept", "confirm"] {
        let output = bperf().args([command, "--help"]).output().unwrap();
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(
            help.contains("[default: latest]"),
            "{command} help:\n{help}"
        );
        assert!(help.contains("unique ID prefix"), "{command} help:\n{help}");
    }

    let history = bperf().args(["history", "--help"]).output().unwrap();
    assert!(history.status.success());
    let history = String::from_utf8(history.stdout).unwrap();
    assert!(
        history.contains("Omit it to use the latest measured benchmark"),
        "history help:\n{history}"
    );
}

#[test]
fn show_and_accept_document_benchmark_scoping() {
    for command in ["show", "accept"] {
        let output = bperf().args([command, "--help"]).output().unwrap();
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("--benchmark <ID>"), "{command} help:\n{help}");
        assert!(
            help.contains("Restrict cycle selection to one benchmark stream"),
            "{command} help:\n{help}"
        );
    }
}

#[test]
fn one_data_directory_relocates_measurement_state() {
    let temporary = tempdir().unwrap();
    let data = temporary.path().join("bperf-data");
    let output = bperf()
        .arg("--data-dir")
        .arg(&data)
        .args(["plan", "--final-samples", "20", "--json"])
        .arg(example("browser-benchmark.yaml"))
        .arg(example("browser-variant-baseline.yaml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let measurement_root =
        fs::canonicalize(PathBuf::from(plan["measurement_root"].as_str().unwrap())).unwrap();
    let measurements = fs::canonicalize(data.join("measurements")).unwrap();
    assert!(measurement_root.starts_with(measurements));
}
