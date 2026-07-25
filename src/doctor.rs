use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::browser_lab::{BrowserLab, BrowserLabConfig, CaptureEvidence, Engine};

pub struct DoctorOptions {
    pub engines: Vec<Engine>,
    pub artifact_root: PathBuf,
    pub node: Option<PathBuf>,
    pub sidecar: Option<PathBuf>,
    pub json: bool,
}

pub fn run(options: DoctorOptions) -> Result<()> {
    if options.engines.is_empty() {
        bail!("at least one engine must be requested");
    }

    let run_id = unique_run_id()?;
    let run_root = options.artifact_root.join(&run_id);
    fs::create_dir_all(&run_root)
        .with_context(|| format!("failed to create {}", run_root.display()))?;
    let run_root = fs::canonicalize(&run_root)
        .with_context(|| format!("failed to resolve {}", run_root.display()))?;

    let mut lab_config = BrowserLabConfig::discover()?;
    if let Some(node) = options.node {
        lab_config = lab_config.with_node(node);
    }
    if let Some(sidecar) = options.sidecar {
        lab_config = lab_config.with_sidecar(sidecar);
    }
    let mut browser_lab = BrowserLab::start(lab_config)?;
    let mut results = Vec::with_capacity(options.engines.len());

    for engine in options.engines {
        eprintln!("[doctor] proving {engine}");
        let result = browser_lab.probe(engine, &run_root)?;
        eprintln!("[doctor] {engine} passed");
        results.push(result);
    }

    browser_lab.finish()?;
    let summary = DoctorSummary {
        schema_version: 1,
        run_id,
        verdict: "supported",
        artifact_root: run_root.to_string_lossy().into_owned(),
        engines: results,
    };
    let encoded = serde_json::to_string_pretty(&summary)?;
    let summary_path = run_root.join("summary.json");
    fs::write(&summary_path, format!("{encoded}\n"))
        .with_context(|| format!("failed to write {}", summary_path.display()))?;

    if options.json {
        println!("{encoded}");
    } else {
        println!("bperf doctor: supported");
        for result in &summary.engines {
            println!(
                "  {:<8} cpu + heap + flamegraph (pid {}, {})",
                result.engine, result.browser.root_pid, result.browser.version
            );
        }
        println!("summary: {}", summary_path.display());
    }
    Ok(())
}

fn unique_run_id() -> Result<String> {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    Ok(format!("doctor-{milliseconds}-{}", std::process::id()))
}

#[derive(Debug, Serialize)]
struct DoctorSummary {
    schema_version: u32,
    run_id: String,
    verdict: &'static str,
    artifact_root: String,
    engines: Vec<CaptureEvidence>,
}
