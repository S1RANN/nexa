use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nexa_embed::{DevelopmentCompileRequest, DevelopmentCompiler, DevelopmentConfig, SourceHash};
use serde_json::json;

use crate::{DiagnosticFormat, project, render_engine_diagnostics};

#[derive(Default)]
struct WatchedPackage {
    observed: Option<SourceHash>,
    stable_scans: u8,
    processed: Option<SourceHash>,
    generation: u64,
}

#[allow(clippy::too_many_lines)]
pub fn dev_command(arguments: &[String], format: DiagnosticFormat) -> Result<(), String> {
    let mut project_path = None;
    let mut once = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--project" => {
                project_path = Some(PathBuf::from(
                    arguments
                        .get(index + 1)
                        .ok_or("missing value for `--project`")?,
                ));
                index += 2;
            }
            "--once" => {
                once = true;
                index += 1;
            }
            option => return Err(format!("unknown dev option `{option}`")),
        }
    }
    let project_path = project_path.ok_or("usage: nexa dev --project nexa.dev.toml")?;
    let mut project = project::LoadedProject::load(&project_path)?;
    let config = DevelopmentConfig {
        auto_reload: false,
        scan_interval_ticks: 1,
        stable_scan_count: 2,
        ..DevelopmentConfig::default()
    };
    let mut compiler = DevelopmentCompiler::start(&config).map_err(str::to_owned)?;
    let running = Arc::new(AtomicBool::new(true));
    let signal = Arc::clone(&running);
    ctrlc::set_handler(move || signal.store(false, Ordering::Release))
        .map_err(|error| format!("could not install Ctrl+C handler: {error}"))?;
    let mut watched = BTreeMap::<PathBuf, WatchedPackage>::new();
    let mut contract_hash = stable_hash(&project.contract_source);
    let mut successful = BTreeMap::new();
    let mut scans = 0_u64;
    while running.load(Ordering::Acquire) {
        let current_contract = std::fs::read_to_string(project.root.join(&project.config.contract))
            .map_err(|error| format!("could not read Host contract: {error}"))?;
        let current_contract_hash = stable_hash(&current_contract);
        if current_contract_hash != contract_hash {
            emit_event(
                format,
                "host-rebuild-required",
                None,
                0,
                "Host contract changed; rebuild the Rust Host before Runtime reload",
            );
            contract_hash = current_contract_hash;
            if let Ok(updated) = project::LoadedProject::load(&project_path) {
                project = updated;
            }
        }
        for directory in project.package_directories()? {
            let loaded = project::load_package_candidate(&directory);
            let (source_id, candidate) = match loaded {
                Ok(candidate) => candidate,
                Err(diagnostic) => {
                    render_engine_diagnostics(&[diagnostic], format)?;
                    continue;
                }
            };
            let hash = SourceHash(nexa_core::StableId::from_parts(&[
                &candidate.manifest_source,
                "\0",
                &candidate.entry_source,
            ]));
            let state = watched.entry(directory.clone()).or_default();
            if state.processed == Some(hash) {
                continue;
            }
            if state.observed != Some(hash) {
                state.observed = Some(hash);
                state.stable_scans = 1;
                emit_event(
                    format,
                    "change-detected",
                    Some(candidate.manifest.id.as_str()),
                    state.generation.saturating_add(1),
                    &directory.display().to_string(),
                );
                continue;
            }
            state.stable_scans = state.stable_scans.saturating_add(1);
            if state.stable_scans < config.stable_scan_count {
                continue;
            }
            state.generation = state.generation.saturating_add(1);
            state.processed = Some(hash);
            emit_event(
                format,
                "compile-queued",
                Some(candidate.manifest.id.as_str()),
                state.generation,
                "stable source snapshot",
            );
            for (package_id, generation, _) in compiler.submit(DevelopmentCompileRequest {
                package_id: candidate.manifest.id.clone(),
                source_id,
                generation: state.generation,
                candidate,
                idl: project.idl.clone(),
                required_exports: project.required_exports.clone(),
            }) {
                emit_event(
                    format,
                    "candidate-superseded",
                    Some(package_id.as_str()),
                    generation,
                    "newer source generation replaced this queued candidate",
                );
            }
        }
        std::thread::sleep(Duration::from_millis(25));
        for result in compiler.poll() {
            match result.result {
                Ok(artifact) => {
                    successful.insert(result.package_id.clone(), artifact);
                    emit_event(
                        format,
                        "candidate-ready",
                        Some(result.package_id.as_str()),
                        result.generation,
                        "latest successful Candidate retained",
                    );
                }
                Err(diagnostic) => {
                    emit_event(
                        format,
                        "compile-failed",
                        Some(result.package_id.as_str()),
                        result.generation,
                        "Last Known Good Candidate retained",
                    );
                    render_engine_diagnostics(&[diagnostic], format)?;
                }
            }
        }
        scans = scans.saturating_add(1);
        if once && scans >= 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(75));
    }
    compiler.shutdown();
    emit_event(
        format,
        "shutdown",
        None,
        0,
        &format!("{} successful Candidates retained", successful.len()),
    );
    Ok(())
}

fn stable_hash(source: &str) -> nexa_core::StableId {
    nexa_core::StableId::from_name(source)
}

fn emit_event(
    format: DiagnosticFormat,
    kind: &str,
    package_id: Option<&str>,
    generation: u64,
    message: &str,
) {
    match format {
        DiagnosticFormat::Human => {
            let package = package_id.map_or_else(String::new, |id| format!(" [{id}]"));
            println!("{kind}{package} generation={generation}: {message}");
        }
        DiagnosticFormat::Json | DiagnosticFormat::Ndjson => println!(
            "{}",
            serde_json::to_string(&json!({
                "schema": 1,
                "type": "development-event",
                "event": kind,
                "packageId": package_id,
                "candidateGeneration": generation,
                "message": message,
            }))
            .expect("development event JSON serialization does not fail")
        ),
    }
}
