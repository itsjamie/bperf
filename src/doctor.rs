use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use bperf_browser::lab::{BrowserLab, CaptureEvidence, Engine, GpuApi};
use bperf_runtime::installation::BrowserInstallation;
use serde::Serialize;

pub struct DoctorOptions {
    pub engines: Vec<Engine>,
    pub gpu: Option<GpuApi>,
    pub artifact_root: PathBuf,
    pub runtime: BrowserInstallation,
    pub json: bool,
}

pub fn run(options: DoctorOptions) -> Result<()> {
    let DoctorOptions {
        engines,
        gpu,
        artifact_root,
        runtime,
        json,
    } = options;
    if engines.is_empty() {
        bail!("at least one engine must be requested");
    }
    if let Some(api) = gpu {
        let unsupported: Vec<_> = engines
            .iter()
            .filter(|engine| !engine.supports_gpu(api))
            .map(|engine| engine.as_str())
            .collect();
        if !unsupported.is_empty() {
            let supported: Vec<_> = Engine::ALL
                .into_iter()
                .filter(|engine| engine.supports_gpu(api))
                .map(Engine::as_str)
                .collect();
            bail!(
                "--gpu {api} is not implemented for {}; pass --engine {}",
                unsupported.join(", "),
                supported.join(" or --engine ")
            );
        }
    }

    let run_id = unique_run_id()?;
    let run_root = bperf_storage::create_contained_directory(&artifact_root, Path::new(&run_id))?;

    let results = BrowserLab::run(runtime, |browser_lab| {
        browser_lab.configure_gpu(gpu)?;
        let mut results = Vec::with_capacity(engines.len());
        for engine in engines {
            eprintln!("[doctor] proving {engine}");
            let result = browser_lab.probe(engine, &run_root)?;
            eprintln!("[doctor] {engine} passed");
            results.push(result);
        }
        Ok(results)
    })?;
    let summary = DoctorSummary {
        schema_version: 2,
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
                result.engine, result.browser.root_pid, result.browser.version
            );
            if let Some(gpu) = &result.gpu {
                println!(
                    "  {:<8} {} on {} ({}), anchor median {:.3} ms",
                    "",
                    gpu.api,
                    gpu.renderer,
                    if gpu.software { "software" } else { "hardware" },
                    median(&gpu.anchor.wall_ms)
                );
                for device in &gpu.devices {
                    println!("  {:<8} device {device}", "");
                }
            }
        }
        println!("summary: {}", summary_path.display());
    }
    Ok(())
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted.get(sorted.len() / 2).copied().unwrap_or(f64::NAN)
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
