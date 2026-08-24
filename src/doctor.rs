use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::{BrowserLab, CaptureEvidence, Engine, SamplerOverheadCheck};
use bperf_runtime::installation::BrowserInstallation;
use serde::Serialize;

// The decision policy treats 5% as the minimum effect worth resolving, so a
// sampler that costs more wall time than that would masquerade as one.
const SAMPLER_OVERHEAD_BUDGET_PCT: f64 = 5.0;

pub struct DoctorOptions {
    pub engines: Vec<Engine>,
    pub artifact_root: PathBuf,
    pub runtime: BrowserInstallation,
    pub json: bool,
}

pub fn run(options: DoctorOptions) -> Result<()> {
    let DoctorOptions {
        engines,
        artifact_root,
        runtime,
        json,
    } = options;
    if engines.is_empty() {
        bail!("at least one engine must be requested");
    }

    let run_id = unique_run_id()?;
    let run_root = artifact_root.join(&run_id);
    fs::create_dir_all(&run_root)
        .with_context(|| format!("failed to create {}", run_root.display()))?;
    let run_root = fs::canonicalize(&run_root)
        .with_context(|| format!("failed to resolve {}", run_root.display()))?;

    let results = BrowserLab::run(runtime, |browser_lab| {
        let mut results = Vec::with_capacity(engines.len());
        for engine in engines {
            eprintln!("[doctor] proving {engine}");
            let capture = browser_lab.probe(engine, &run_root)?;
            let sampler_overhead = browser_lab.sampler_overhead(engine)?;
            if let SamplerOverheadCheck::Measured(evidence) = &sampler_overhead {
                let overhead_pct = evidence.overhead_pct();
                if overhead_pct > SAMPLER_OVERHEAD_BUDGET_PCT {
                    eprintln!(
                        "[doctor] warning: {engine} allocation sampler added {overhead_pct:+.1}% \
                         wall time on the doctor workload; the budget is \
                         {SAMPLER_OVERHEAD_BUDGET_PCT}%"
                    );
                }
            }
            eprintln!("[doctor] {engine} passed");
            results.push(EngineReport {
                capture,
                sampler_overhead,
            });
        }
        Ok(results)
    })?;
    let summary = DoctorSummary {
        schema_version: 3,
        run_id,
        verdict: "supported",
        artifact_root: run_root.to_string_lossy().into_owned(),
        engines: results,
    };
    let encoded = serde_json::to_string_pretty(&summary)?;
    let summary_path = run_root.join("summary.json");
    fs::write(&summary_path, format!("{encoded}\n"))
        .with_context(|| format!("failed to write {}", summary_path.display()))?;

    if json {
        println!("{encoded}");
    } else {
        println!("bperf doctor: supported");
        for result in &summary.engines {
            println!(
                "  {:<8} cpu + heap + flamegraph (pid {}, {})",
                result.capture.engine,
                result.capture.browser.root_pid,
                result.capture.browser.version
            );
            match &result.sampler_overhead {
                SamplerOverheadCheck::Measured(evidence) => println!(
                    "  {:<8} sampler overhead {:+.1}% (unsampled {:.3} ms, sampled {:.3} ms)",
                    result.capture.engine,
                    evidence.overhead_pct(),
                    evidence.unsampled_wall_ms,
                    evidence.sampled_wall_ms
                ),
                SamplerOverheadCheck::NotApplicable { reason } => println!(
                    "  {:<8} sampler overhead n/a ({reason})",
                    result.capture.engine
                ),
            }
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
    engines: Vec<EngineReport>,
}

#[derive(Debug, Serialize)]
struct EngineReport {
    #[serde(flatten)]
    capture: CaptureEvidence,
    sampler_overhead: SamplerOverheadCheck,
}
