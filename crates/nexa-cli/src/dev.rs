use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nexa_analysis::{
    CandidateIdentity, DevelopmentCompletionKind, DevelopmentCompletionOutcome,
    DevelopmentCoordinator, DevelopmentCoordinatorConfig, DevelopmentInvalidation,
    DevelopmentQueueOutcome, DevelopmentTerminal, DevelopmentTerminalKind, FingerprintBuilder,
    PackageId, ResolvedBuildInput,
};
use serde_json::json;

use crate::{CliError, CliErrorKind, CliResult, DiagnosticFormat, finish_resolved_build, project};

#[allow(clippy::too_many_lines)]
pub fn dev_command(project_path: &Path, once: bool, format: DiagnosticFormat) -> CliResult<()> {
    let initial_project = project::LoadedProject::load(project_path)?;
    let running = Arc::new(AtomicBool::new(true));
    if !once {
        let signal = Arc::clone(&running);
        ctrlc::set_handler(move || signal.store(false, Ordering::Release)).map_err(|error| {
            CliError::internal(format!("could not install Ctrl+C handler: {error}"))
        })?;
    }

    let mut coordinator = DevelopmentCoordinator::new(DevelopmentCoordinatorConfig {
        stable_scan_count: 2,
        queue_capacity: 16,
        retain_terminal_generations: 128,
    });
    let mut build_session = nexa::PackageBuildSession::new();
    let mut snapshots = BTreeMap::<CandidateIdentity, project::ResolvedBuild>::new();
    let mut awaiting = BTreeSet::<CandidateIdentity>::new();
    let mut directories = BTreeMap::<PackageId, PathBuf>::new();
    let mut successful = BTreeMap::<PackageId, CandidateIdentity>::new();
    let mut successful_inputs = BTreeMap::<PackageId, Arc<ResolvedBuildInput>>::new();
    let mut missing = BTreeSet::<PackageId>::new();
    let mut contract_identity = nexa::contract_fingerprint(&initial_project.contract);
    let mut rendered_failure = false;
    let mut deferred_error = None;

    'watch: while running.load(Ordering::Acquire) {
        let current_project = match project::LoadedProject::load(project_path) {
            Ok(project) => project,
            Err(error) => {
                invalidate_all(
                    &mut coordinator,
                    &mut snapshots,
                    &mut awaiting,
                    &directories,
                    format,
                    DevelopmentInvalidation::Transient,
                    &error.to_string(),
                );
                if once {
                    defer_error(&mut deferred_error, error);
                    break 'watch;
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let current_contract_identity = nexa::contract_fingerprint(&current_project.contract);
        if current_contract_identity != contract_identity {
            contract_identity = current_contract_identity;
            emit_event(
                format,
                "host-rebuild-required",
                None,
                None,
                "Host Contract changed; rebuild the Rust Host before Runtime reload",
            );
        }

        let current_builds = match current_project.resolved_builds(true) {
            Ok(builds) => builds,
            Err(error) => {
                invalidate_all(
                    &mut coordinator,
                    &mut snapshots,
                    &mut awaiting,
                    &directories,
                    format,
                    DevelopmentInvalidation::Transient,
                    &error.to_string(),
                );
                if once {
                    defer_error(&mut deferred_error, error);
                    break 'watch;
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        let mut seen = BTreeSet::new();
        let mut current_scan_settled = true;
        for build in current_builds {
            let package_id = build.package_id().clone();
            let directory = build.root.directory.clone();
            directories.insert(package_id.clone(), directory.clone());
            seen.insert(package_id.clone());
            missing.remove(&package_id);
            let observation = coordinator.observe(package_id, build.build_fingerprint);
            current_scan_settled &= observation.matched_active || observation.matched_terminal;
            for terminal in observation.terminals {
                consume_terminal(
                    &terminal,
                    &mut snapshots,
                    &mut awaiting,
                    &directories,
                    format,
                );
            }
            if let Some(identity) = observation.identity {
                snapshots.insert(identity.clone(), build);
                if observation.change_detected {
                    emit_event(
                        format,
                        "change-detected",
                        Some(&identity),
                        Some(&directory),
                        "complete Package/dependency/lock/contract snapshot changed",
                    );
                }
                if observation.stable {
                    awaiting.insert(identity.clone());
                    emit_event(
                        format,
                        "change-stabilized",
                        Some(&identity),
                        Some(&directory),
                        "complete Build Fingerprint was stable for two scans",
                    );
                }
            }
        }

        let known = coordinator
            .inspection()
            .packages
            .keys()
            .filter(|package| !seen.contains(*package))
            .cloned()
            .collect::<Vec<_>>();
        for package_id in known {
            if missing.insert(package_id.clone()) {
                invalidate_candidate(
                    &mut coordinator,
                    &package_id,
                    &mut snapshots,
                    &mut awaiting,
                    &directories,
                    format,
                    DevelopmentInvalidation::SourceRemoval,
                    &format!("Package `{package_id}` left the configured source set"),
                );
                successful.remove(&package_id);
                successful_inputs.remove(&package_id);
                directories.remove(&package_id);
            }
        }

        loop {
            retry_awaiting(&mut coordinator, &mut awaiting, &directories, format);
            while let Some(identity) = coordinator.start_next() {
                let Some(build) = snapshots.get(&identity).cloned() else {
                    defer_error(
                        &mut deferred_error,
                        CliError::internal(format!(
                            "DevelopmentCoordinator started unknown Candidate {identity:?}"
                        )),
                    );
                    break 'watch;
                };
                let directory = build.root.directory.clone();
                emit_event(
                    format,
                    "compile-started",
                    Some(&identity),
                    Some(&directory),
                    "stable immutable ResolvedBuildInput",
                );
                let compilation = build.compile_with_session(
                    &mut build_session,
                    identity.generation,
                    None,
                    build.host_contract.required_entrypoints.as_ref(),
                    false,
                );

                let refreshed_project = match project::LoadedProject::load(project_path) {
                    Ok(project) => project,
                    Err(error) => {
                        invalidate_candidate(
                            &mut coordinator,
                            &identity.package_id,
                            &mut snapshots,
                            &mut awaiting,
                            &directories,
                            format,
                            DevelopmentInvalidation::Transient,
                            &format!("Build input became invalid before commit: {error}"),
                        );
                        if once {
                            defer_error(&mut deferred_error, error);
                            break 'watch;
                        }
                        continue;
                    }
                };
                let refreshed_builds = match refreshed_project.resolved_builds(true) {
                    Ok(builds) => builds,
                    Err(error) => {
                        invalidate_candidate(
                            &mut coordinator,
                            &identity.package_id,
                            &mut snapshots,
                            &mut awaiting,
                            &directories,
                            format,
                            DevelopmentInvalidation::Transient,
                            &format!("Build input became invalid before commit: {error}"),
                        );
                        if once {
                            defer_error(&mut deferred_error, error);
                            break 'watch;
                        }
                        continue;
                    }
                };
                let refreshed = refreshed_builds.into_iter().find(|candidate| {
                    candidate.package_id() == &identity.package_id
                        && candidate.root.directory == directory
                });
                let Some(refreshed) = refreshed else {
                    invalidate_candidate(
                        &mut coordinator,
                        &identity.package_id,
                        &mut snapshots,
                        &mut awaiting,
                        &directories,
                        format,
                        DevelopmentInvalidation::SourceRemoval,
                        "Package was removed or renamed during compilation",
                    );
                    continue;
                };

                let completion_kind = match &compilation {
                    Ok(_) => DevelopmentCompletionKind::Compiled,
                    Err(project::BuildCompileError::Facade(nexa::PackageBuildError::Verify(_))) => {
                        DevelopmentCompletionKind::VerifyFailed
                    }
                    Err(_) => DevelopmentCompletionKind::CompileFailed,
                };
                let retained_host_contract_changed = successful_inputs
                    .get(&identity.package_id)
                    .is_some_and(|retained| {
                        retained.canonical_host_contract != build.input.canonical_host_contract
                    });
                let outcome = coordinator.complete(
                    identity.clone(),
                    &build.input,
                    &refreshed.input,
                    completion_kind,
                    retained_host_contract_changed,
                );
                match outcome {
                    DevelopmentCompletionOutcome::Accepted(terminal) => {
                        consume_terminal(
                            &terminal,
                            &mut snapshots,
                            &mut awaiting,
                            &directories,
                            format,
                        );
                        match compilation {
                            Ok(_) => {
                                successful.insert(identity.package_id.clone(), identity.clone());
                                successful_inputs
                                    .insert(identity.package_id.clone(), Arc::clone(&build.input));
                                coordinator.retain_active(
                                    identity.package_id.clone(),
                                    identity.build_fingerprint,
                                );
                                emit_event(
                                    format,
                                    "candidate-ready",
                                    Some(&identity),
                                    Some(&directory),
                                    "latest successful Candidate retained",
                                );
                            }
                            Err(error) => {
                                let summary = match &error {
                                    project::BuildCompileError::Facade(
                                        nexa::PackageBuildError::AnalysisFailed(batch)
                                        | nexa::PackageBuildError::CompileFailed(batch),
                                    ) => minimal_diagnostic_summary(batch),
                                    _ => None,
                                };
                                let error = finish_resolved_build(&build, Err(error), format)
                                    .expect_err("an accepted failed compilation remains an error");
                                if error.kind == CliErrorKind::WorkerIoOrInternal {
                                    defer_error(&mut deferred_error, error);
                                    break 'watch;
                                }
                                rendered_failure = true;
                                let message = failure_event_message(&error, summary.as_deref());
                                emit_event(
                                    format,
                                    if terminal.kind == DevelopmentTerminalKind::VerifyFailed {
                                        "verify-failed"
                                    } else {
                                        "compile-failed"
                                    },
                                    Some(&identity),
                                    Some(&directory),
                                    &message,
                                );
                            }
                        }
                    }
                    DevelopmentCompletionOutcome::Rejected {
                        terminal,
                        freshness,
                    } => {
                        consume_terminal(
                            &terminal,
                            &mut snapshots,
                            &mut awaiting,
                            &directories,
                            format,
                        );
                        emit_event(
                            format,
                            if terminal.kind == DevelopmentTerminalKind::RejectedHostContractChange
                            {
                                "host-rebuild-required"
                            } else {
                                "candidate-superseded"
                            },
                            Some(&identity),
                            Some(&directory),
                            &format!("Candidate commit rejected by freshness gate: {freshness:?}"),
                        );
                    }
                    DevelopmentCompletionOutcome::AlreadyTerminal { kind, .. } => {
                        defer_error(
                            &mut deferred_error,
                            CliError::internal(format!(
                                "duplicate Development Candidate terminal: {kind:?}"
                            )),
                        );
                        break 'watch;
                    }
                    DevelopmentCompletionOutcome::Stale(stale) => {
                        snapshots.remove(&stale);
                        awaiting.remove(&stale);
                        emit_event(
                            format,
                            "candidate-superseded",
                            Some(&stale),
                            Some(&directory),
                            "Candidate was no longer desired at completion",
                        );
                    }
                }
            }
            if awaiting.is_empty() {
                break;
            }
        }

        if once && current_scan_settled && awaiting.is_empty() {
            let inspection = coordinator.inspection();
            if inspection.generations_without_terminal == 0
                && inspection.queued_candidates == 0
                && inspection.in_flight_candidates == 0
            {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(if once { 10 } else { 100 }));
    }

    for terminal in coordinator.shutdown() {
        consume_terminal(
            &terminal,
            &mut snapshots,
            &mut awaiting,
            &directories,
            format,
        );
    }
    let inspection = coordinator.inspection();
    if let Some(error) = deferred_error {
        return Err(error);
    }
    if inspection.generations_without_terminal != 0 || inspection.duplicate_terminals != 0 {
        return Err(CliError::internal(format!(
            "DevelopmentCoordinator terminal invariant failed: {} missing, {} duplicate",
            inspection.generations_without_terminal, inspection.duplicate_terminals
        )));
    }
    emit_event(
        format,
        "shutdown",
        None,
        None,
        &format!("{} successful Candidates retained", successful.len()),
    );
    if rendered_failure {
        Err(CliError::rendered_diagnostic(
            "one or more development Candidates failed",
        ))
    } else {
        Ok(())
    }
}

/// Records the first root-cause error and ignores later ones so a single `dev --once`/event-loop
/// run cannot overwrite the original failure with a secondary diagnosis.
fn defer_error(deferred_error: &mut Option<CliError>, error: CliError) {
    if deferred_error.is_none() {
        *deferred_error = Some(error);
    }
}

/// Minimal diagnostic summary for a failed Candidate event: the first error-severity diagnostic's
/// code and message, plus a count of any remaining diagnostics. Returns `None` only when the batch
/// carries no diagnostics at all.
fn minimal_diagnostic_summary(batch: &nexa::DiagnosticBatch) -> Option<String> {
    let diagnostics = batch.diagnostics();
    let first = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.severity == nexa::Severity::Error)
        .or_else(|| diagnostics.first())?;
    let code = first.code.as_str();
    let message = first.message.trim();
    let summary = if message.is_empty() {
        code.to_owned()
    } else {
        format!("{code}: {message}")
    };
    let remaining = diagnostics.len().saturating_sub(1);
    Some(if remaining > 0 {
        format!("{summary} (and {remaining} more)")
    } else {
        summary
    })
}

/// Event message for `compile-failed`/`verify-failed`: always carries a minimal diagnostic
/// summary, preserving the "Last Known Good Candidate retained" notice for diagnostics that were
/// already rendered separately.
fn failure_event_message(error: &CliError, batch_summary: Option<&str>) -> String {
    match (error.already_rendered(), batch_summary) {
        (true, Some(summary)) => format!("{summary}; Last Known Good Candidate retained"),
        (true, None) => format!("{error}; Last Known Good Candidate retained"),
        (false, summary) => summary.map_or_else(|| error.to_string(), str::to_owned),
    }
}

fn retry_awaiting(
    coordinator: &mut DevelopmentCoordinator,
    awaiting: &mut BTreeSet<CandidateIdentity>,
    directories: &BTreeMap<PackageId, PathBuf>,
    format: DiagnosticFormat,
) {
    for identity in awaiting.iter().cloned().collect::<Vec<_>>() {
        match coordinator.enqueue(identity.clone()) {
            DevelopmentQueueOutcome::Accepted => {
                awaiting.remove(&identity);
                emit_event(
                    format,
                    "compile-queued",
                    Some(&identity),
                    directories.get(&identity.package_id),
                    "stable Candidate admitted to the bounded compile queue",
                );
            }
            DevelopmentQueueOutcome::AlreadyQueued => {
                awaiting.remove(&identity);
            }
            DevelopmentQueueOutcome::Backpressured(_) => {
                emit_event(
                    format,
                    "compile-backpressured",
                    Some(&identity),
                    directories.get(&identity.package_id),
                    "bounded compile queue is full; Candidate remains pending",
                );
            }
            DevelopmentQueueOutcome::Stale(stale) => {
                awaiting.remove(&stale);
            }
        }
    }
}

fn invalidate_all(
    coordinator: &mut DevelopmentCoordinator,
    snapshots: &mut BTreeMap<CandidateIdentity, project::ResolvedBuild>,
    awaiting: &mut BTreeSet<CandidateIdentity>,
    directories: &BTreeMap<PackageId, PathBuf>,
    format: DiagnosticFormat,
    reason: DevelopmentInvalidation,
    message: &str,
) {
    let packages = coordinator
        .inspection()
        .packages
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for package in packages {
        invalidate_candidate(
            coordinator,
            &package,
            snapshots,
            awaiting,
            directories,
            format,
            reason,
            message,
        );
    }
    emit_event(format, "build-input-invalid", None, None, message);
}

#[allow(clippy::too_many_arguments)]
fn invalidate_candidate(
    coordinator: &mut DevelopmentCoordinator,
    package: &PackageId,
    snapshots: &mut BTreeMap<CandidateIdentity, project::ResolvedBuild>,
    awaiting: &mut BTreeSet<CandidateIdentity>,
    directories: &BTreeMap<PackageId, PathBuf>,
    format: DiagnosticFormat,
    reason: DevelopmentInvalidation,
    message: &str,
) {
    let inspection = coordinator.inspection();
    let package_inspection = inspection.packages.get(package);
    let next_generation =
        package_inspection.map_or(1, |package| package.latest_generation.saturating_add(1));
    let active = package_inspection.and_then(|package| package.active_build_fingerprint);
    let mut terminals = coordinator.invalidate(package, reason);
    if reason == DevelopmentInvalidation::SourceRemoval && terminals.is_empty() {
        let mut fingerprint = FingerprintBuilder::new("nexa.cli.source-missing", 1);
        fingerprint.field_str("package", package.as_str());
        fingerprint.field_u64("generation", next_generation);
        if let Some(active) = active {
            fingerprint.field_bytes("active-build", active.as_bytes());
        }
        let source_missing =
            nexa_analysis::BuildFingerprint::from_bytes(fingerprint.finish_bytes());
        let (identity, superseded) = coordinator.begin(package.clone(), source_missing);
        debug_assert!(superseded.is_empty());
        terminals = coordinator.invalidate(package, DevelopmentInvalidation::SourceRemoval);
        debug_assert_eq!(
            terminals.first().map(|terminal| &terminal.identity),
            Some(&identity)
        );
    }
    for terminal in terminals {
        consume_terminal(&terminal, snapshots, awaiting, directories, format);
    }
    if reason == DevelopmentInvalidation::Transient {
        emit_event(
            format,
            "candidate-superseded",
            None,
            directories.get(package),
            message,
        );
    }
}

fn consume_terminal(
    terminal: &DevelopmentTerminal,
    snapshots: &mut BTreeMap<CandidateIdentity, project::ResolvedBuild>,
    awaiting: &mut BTreeSet<CandidateIdentity>,
    directories: &BTreeMap<PackageId, PathBuf>,
    format: DiagnosticFormat,
) {
    snapshots.remove(&terminal.identity);
    awaiting.remove(&terminal.identity);
    if matches!(
        terminal.kind,
        DevelopmentTerminalKind::SupersededBeforeCompile
            | DevelopmentTerminalKind::SupersededInFlight
            | DevelopmentTerminalKind::CancelledByInvalidation
            | DevelopmentTerminalKind::CancelledBySourceRemoval
            | DevelopmentTerminalKind::CancelledByShutdown
    ) {
        emit_event(
            format,
            match terminal.kind {
                DevelopmentTerminalKind::CancelledBySourceRemoval => "source-missing",
                DevelopmentTerminalKind::CancelledByShutdown => "candidate-cancelled",
                _ => "candidate-superseded",
            },
            Some(&terminal.identity),
            directories.get(&terminal.identity.package_id),
            &format!("Development terminal: {:?}", terminal.kind),
        );
    }
}

fn emit_event(
    format: DiagnosticFormat,
    kind: &str,
    identity: Option<&CandidateIdentity>,
    directory: Option<&PathBuf>,
    message: &str,
) {
    match format {
        DiagnosticFormat::Human => {
            let package = identity.map_or_else(String::new, |identity| {
                format!(
                    " [{}] generation={} fingerprint={}",
                    identity.package_id, identity.generation, identity.build_fingerprint
                )
            });
            println!("{kind}{package}: {message}");
        }
        DiagnosticFormat::Json | DiagnosticFormat::Ndjson => println!(
            "{}",
            serde_json::to_string(&json!({
                "schema": 1,
                "type": "development-event",
                "event": kind,
                "packageId": identity.map(|identity| identity.package_id.as_str()),
                "candidateGeneration": identity.map(|identity| identity.generation),
                "buildFingerprint": identity.map(|identity| identity.build_fingerprint),
                "directory": directory,
                "message": message,
            }))
            .expect("development event JSON serialization does not fail")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defer_error_keeps_the_first_root_cause() {
        let mut deferred = None;
        defer_error(&mut deferred, CliError::environment("first root cause"));
        defer_error(&mut deferred, CliError::internal("later secondary failure"));
        assert_eq!(
            deferred.map(|error| error.to_string()).as_deref(),
            Some("first root cause")
        );
    }

    #[test]
    fn minimal_diagnostic_summary_reports_first_error_and_remaining_count() {
        let sources = nexa::SourceSnapshotRegistry::builder().build();
        let mut batch = nexa::DiagnosticBatch::with_default_limits(sources);
        batch.push(nexa::LeafDiagnostic::new(
            nexa::ErrorCode::new("NX1002"),
            nexa::Severity::Error,
            "unclosed delimiter",
        ));
        batch.push(nexa::LeafDiagnostic::new(
            nexa::ErrorCode::new("NX2101"),
            nexa::Severity::Error,
            "expected unit, found i32",
        ));
        assert_eq!(
            minimal_diagnostic_summary(&batch).as_deref(),
            Some("NX1002: unclosed delimiter (and 1 more)")
        );
    }

    #[test]
    fn minimal_diagnostic_summary_falls_back_to_first_diagnostic_without_errors() {
        let sources = nexa::SourceSnapshotRegistry::builder().build();
        let mut batch = nexa::DiagnosticBatch::with_default_limits(sources);
        batch.push(nexa::LeafDiagnostic::new(
            nexa::ErrorCode::new("NX2001"),
            nexa::Severity::Warning,
            "unused value",
        ));
        assert_eq!(
            minimal_diagnostic_summary(&batch).as_deref(),
            Some("NX2001: unused value")
        );
    }

    #[test]
    fn minimal_diagnostic_summary_is_none_for_an_empty_batch() {
        let sources = nexa::SourceSnapshotRegistry::builder().build();
        let batch = nexa::DiagnosticBatch::with_default_limits(sources);
        assert_eq!(minimal_diagnostic_summary(&batch), None);
    }

    #[test]
    fn failure_event_message_keeps_summary_when_diagnostics_were_rendered() {
        let error = CliError::rendered_diagnostic("Package analysis failed");
        assert_eq!(
            failure_event_message(&error, Some("NX1002: unclosed delimiter (and 4 more)")),
            "NX1002: unclosed delimiter (and 4 more); Last Known Good Candidate retained"
        );
        assert_eq!(
            failure_event_message(&error, None),
            "Package analysis failed; Last Known Good Candidate retained"
        );
    }

    #[test]
    fn failure_event_message_keeps_unrendered_error_text() {
        let error = CliError::diagnostic("package verification failed");
        assert_eq!(
            failure_event_message(&error, None),
            "package verification failed"
        );
        assert_eq!(
            failure_event_message(&error, Some("NX1002: unclosed delimiter")),
            "NX1002: unclosed delimiter"
        );
    }
}
