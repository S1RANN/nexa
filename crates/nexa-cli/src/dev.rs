use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nexa_embed::{
    CandidateTerminal, CompileJob, DevelopmentCompileRequest, DevelopmentCompiler,
    DevelopmentConfig, EnqueueOutcome, PackageId, SourceHash, WorkerEvent,
};
use serde_json::json;

use crate::{DiagnosticFormat, project, render_engine_diagnostics};

#[derive(Default)]
struct WatchedPackage {
    observed_hash: Option<SourceHash>,
    stable_hash: Option<SourceHash>,
    queued_hash: Option<SourceHash>,
    terminal_hash: Option<SourceHash>,
    stable_scans: u8,
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
    let mut awaiting = BTreeMap::<PackageId, CompileJob>::new();
    let mut contract_hash = stable_hash(&project.contract_source);
    let mut successful = BTreeMap::new();
    let mut scans = 0_u64;
    while running.load(Ordering::Acquire) {
        retry_awaiting(&compiler, &mut awaiting, &mut watched, format)?;
        emit_worker_events(&compiler, format);

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
        for package in project.package_directories()? {
            let loaded = project::load_package_candidate(
                &package.directory,
                &package.source_id,
                &package.policy,
            );
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
            let state = watched.entry(package.directory.clone()).or_default();
            if state.terminal_hash == Some(hash) || state.queued_hash == Some(hash) {
                continue;
            }
            if state.observed_hash != Some(hash) {
                state.observed_hash = Some(hash);
                state.stable_scans = 1;
                emit_event(
                    format,
                    "change-detected",
                    Some(candidate.manifest.id.as_str()),
                    state.generation.saturating_add(1),
                    &package.directory.display().to_string(),
                );
                continue;
            }
            state.stable_scans = state.stable_scans.saturating_add(1);
            if state.stable_scans < config.stable_scan_count || state.stable_hash == Some(hash) {
                continue;
            }
            state.stable_hash = Some(hash);
            state.generation = state.generation.saturating_add(1);
            let package_id = candidate.manifest.id.clone();
            let generation = state.generation;
            let request = DevelopmentCompileRequest {
                package_id: package_id.clone(),
                source_id,
                generation,
                candidate,
                idl: project.idl.clone(),
                required_exports: project.required_exports.clone(),
            };
            handle_enqueue(
                compiler.submit(request),
                &package_id,
                generation,
                hash,
                &package.directory,
                &mut watched,
                &mut awaiting,
                format,
            )?;
        }
        std::thread::sleep(Duration::from_millis(25));
        emit_worker_events(&compiler, format);
        process_terminals(compiler.poll(), &mut watched, &mut successful, format)?;
        scans = scans.saturating_add(1);
        if once && scans >= 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(75));
    }
    let terminals = compiler.shutdown();
    process_terminals(terminals, &mut watched, &mut successful, format)?;
    emit_event(
        format,
        "shutdown",
        None,
        0,
        &format!("{} successful Candidates retained", successful.len()),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_enqueue(
    outcome: EnqueueOutcome,
    package_id: &PackageId,
    generation: u64,
    hash: SourceHash,
    directory: &PathBuf,
    watched: &mut BTreeMap<PathBuf, WatchedPackage>,
    awaiting: &mut BTreeMap<PackageId, CompileJob>,
    format: DiagnosticFormat,
) -> Result<(), String> {
    match outcome {
        EnqueueOutcome::Accepted => {
            let state = watched
                .get_mut(directory)
                .ok_or("development state disappeared before enqueue")?;
            state.queued_hash = Some(hash);
            emit_event(
                format,
                "compile-queued",
                Some(package_id.as_str()),
                generation,
                "stable source snapshot",
            );
        }
        EnqueueOutcome::ReplacedPending { terminal, .. } => {
            mark_terminal(watched, terminal.data().source_hash);
            emit_terminal_event(&terminal, format);
            let state = watched
                .get_mut(directory)
                .ok_or("development state disappeared before enqueue")?;
            state.queued_hash = Some(hash);
            emit_event(
                format,
                "compile-queued",
                Some(package_id.as_str()),
                generation,
                "newest stable source replaced this Package's pending Candidate",
            );
        }
        EnqueueOutcome::Backpressured { job } => {
            emit_event(
                format,
                "compile-backpressured",
                Some(package_id.as_str()),
                generation,
                "Worker queue is full; the Candidate will be retried",
            );
            awaiting.insert(package_id.clone(), job);
        }
        EnqueueOutcome::Stopping { .. } => {
            return Err("development compiler stopped while accepting a Candidate".into());
        }
    }
    Ok(())
}

fn retry_awaiting(
    compiler: &DevelopmentCompiler,
    awaiting: &mut BTreeMap<PackageId, CompileJob>,
    watched: &mut BTreeMap<PathBuf, WatchedPackage>,
    format: DiagnosticFormat,
) -> Result<(), String> {
    let jobs = std::mem::take(awaiting);
    for (package_id, job) in jobs {
        let generation = job.generation;
        let hash = job.source_hash;
        match compiler.retry(job) {
            EnqueueOutcome::Accepted => {
                if let Some(state) = watched
                    .values_mut()
                    .find(|state| state.stable_hash == Some(hash))
                {
                    state.queued_hash = Some(hash);
                }
                emit_event(
                    format,
                    "compile-queued",
                    Some(package_id.as_str()),
                    generation,
                    "backpressured Candidate accepted",
                );
            }
            EnqueueOutcome::ReplacedPending { terminal, .. } => {
                mark_terminal(watched, terminal.data().source_hash);
                emit_terminal_event(&terminal, format);
                if let Some(state) = watched
                    .values_mut()
                    .find(|state| state.stable_hash == Some(hash))
                {
                    state.queued_hash = Some(hash);
                }
            }
            EnqueueOutcome::Backpressured { job } => {
                awaiting.insert(package_id, job);
            }
            EnqueueOutcome::Stopping { .. } => {
                return Err("development compiler stopped while retrying a Candidate".into());
            }
        }
    }
    Ok(())
}

fn emit_worker_events(compiler: &DevelopmentCompiler, format: DiagnosticFormat) {
    for event in compiler.poll_events() {
        match event {
            WorkerEvent::CompileStarted {
                package_id,
                generation,
                queue_duration,
                ..
            } => emit_event(
                format,
                "compile-started",
                Some(package_id.as_str()),
                generation,
                &format!("queued for {} μs", queue_duration.as_micros()),
            ),
        }
    }
}

fn process_terminals(
    terminals: Vec<CandidateTerminal>,
    watched: &mut BTreeMap<PathBuf, WatchedPackage>,
    successful: &mut BTreeMap<PackageId, nexa_embed::CompiledPackageArtifact>,
    format: DiagnosticFormat,
) -> Result<(), String> {
    for terminal in terminals {
        mark_terminal(watched, terminal.data().source_hash);
        match terminal {
            CandidateTerminal::Compiled {
                data, compilation, ..
            } => {
                successful.insert(data.package_id.clone(), compilation.artifact);
                emit_event(
                    format,
                    "candidate-ready",
                    Some(data.package_id.as_str()),
                    data.generation,
                    "latest successful Candidate retained",
                );
            }
            CandidateTerminal::CompileFailed {
                data, diagnostic, ..
            }
            | CandidateTerminal::VerifyFailed {
                data, diagnostic, ..
            } => {
                emit_event(
                    format,
                    "compile-failed",
                    Some(data.package_id.as_str()),
                    data.generation,
                    "Last Known Good Candidate retained",
                );
                render_engine_diagnostics(&[diagnostic], format)?;
            }
            terminal => emit_terminal_event(&terminal, format),
        }
    }
    Ok(())
}

fn mark_terminal(watched: &mut BTreeMap<PathBuf, WatchedPackage>, hash: SourceHash) {
    if let Some(state) = watched
        .values_mut()
        .find(|state| state.queued_hash == Some(hash) || state.stable_hash == Some(hash))
    {
        state.terminal_hash = Some(hash);
        if state.queued_hash == Some(hash) {
            state.queued_hash = None;
        }
    }
}

fn emit_terminal_event(terminal: &CandidateTerminal, format: DiagnosticFormat) {
    let data = terminal.data();
    emit_event(
        format,
        match terminal.kind() {
            nexa_embed::CandidateTerminalKind::SupersededBeforeCompile
            | nexa_embed::CandidateTerminalKind::SupersededAfterCompile => "candidate-superseded",
            nexa_embed::CandidateTerminalKind::CancelledByDisable
            | nexa_embed::CandidateTerminalKind::CancelledBySourceRemoval
            | nexa_embed::CandidateTerminalKind::CancelledByShutdown => "candidate-cancelled",
            nexa_embed::CandidateTerminalKind::RejectedHostContractChange => {
                "host-rebuild-required"
            }
            nexa_embed::CandidateTerminalKind::Compiled => "candidate-ready",
            nexa_embed::CandidateTerminalKind::CompileFailed => "compile-failed",
            nexa_embed::CandidateTerminalKind::VerifyFailed => "verify-failed",
        },
        Some(data.package_id.as_str()),
        data.generation,
        &format!("{:?}", terminal.kind()),
    );
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
