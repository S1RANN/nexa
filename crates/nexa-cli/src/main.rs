use std::collections::BTreeSet;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::ValueEnum;
use nexa_machine::{MachineSpec, stable_id_map};
use nexa_model::artifact::{
    MODEL_FAILURE_ARTIFACT_VERSION, ModelFailureArtifact, current_commit_sha,
    write_model_failure_artifact,
};
use nexa_model::explore;
use nexa_model::realm::{RealmEvent, RealmModel};
use nexa_model::system::{
    RealmSystemConfig, SystemConfig, explore_realm_runtime, explore_task_scope,
};
use serde_json::{Value, json};

mod cli;
mod dev;
mod lsp;
mod project;
mod repl;
mod standalone;

const REQUIRED_BASELINE: &[&str] = &[
    "baseline/BASELINE_INDEX.md",
    "baseline/internal/INTERNAL_LANGUAGE_SCOPE.md",
    "baseline/internal/HOST_BINDING.md",
    "baseline/internal/TASK_RUNTIME.md",
    "baseline/internal/RESTART_RELOAD.md",
    "baseline/runtime/TASK_MACHINE.md",
    "baseline/runtime/SCOPE_MACHINE.md",
    "baseline/runtime/MODULE_MACHINE.md",
    "baseline/runtime/HOST_REQUEST_MACHINE.md",
    "baseline/runtime/RESOURCE_MACHINE.md",
    "baseline/runtime/HANDLES.md",
    "baseline/abi/BYTECODE.md",
    "baseline/abi/IDL.md",
    "baseline/abi/RUST_HOST_ABI.md",
    "docs/TESTING.md",
    "docs/ARTIFACT_POLICY.md",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum DiagnosticFormat {
    Human,
    Json,
    Ndjson,
}

impl std::fmt::Display for DiagnosticFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Human => "human",
            Self::Json => "json",
            Self::Ndjson => "ndjson",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CliErrorKind {
    DiagnosticOrTestFailure,
    UsageOrEnvironment,
    WorkerIoOrInternal,
    RuntimeTrap,
}

impl CliErrorKind {
    const fn exit_code(self) -> i32 {
        match self {
            Self::DiagnosticOrTestFailure => 1,
            Self::UsageOrEnvironment => 2,
            Self::WorkerIoOrInternal => 3,
            Self::RuntimeTrap => standalone::TRAP_EXIT_CODE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CliError {
    kind: CliErrorKind,
    message: String,
    already_rendered: bool,
}

impl CliError {
    pub(crate) fn diagnostic(message: impl Into<String>) -> Self {
        Self {
            kind: CliErrorKind::DiagnosticOrTestFailure,
            message: message.into(),
            already_rendered: false,
        }
    }

    pub(crate) fn rendered_diagnostic(message: impl Into<String>) -> Self {
        Self {
            kind: CliErrorKind::DiagnosticOrTestFailure,
            message: message.into(),
            already_rendered: true,
        }
    }

    pub(crate) fn environment(message: impl Into<String>) -> Self {
        Self {
            kind: CliErrorKind::UsageOrEnvironment,
            message: message.into(),
            already_rendered: false,
        }
    }

    pub(crate) fn usage(message: impl Into<String>) -> Self {
        Self::environment(message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: CliErrorKind::WorkerIoOrInternal,
            message: message.into(),
            already_rendered: false,
        }
    }

    pub(crate) fn runtime_trap(message: impl Into<String>) -> Self {
        Self {
            kind: CliErrorKind::RuntimeTrap,
            message: message.into(),
            already_rendered: false,
        }
    }

    const fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }

    pub(crate) const fn already_rendered(&self) -> bool {
        self.already_rendered
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub(crate) type CliResult<T> = Result<T, CliError>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum CommandOutcome {
    Success,
    Failure(CliError),
}

impl CommandOutcome {
    fn from_result(result: CliResult<()>) -> Self {
        match result {
            Ok(()) => Self::Success,
            Err(error) => Self::Failure(error),
        }
    }

    const fn exit_code(&self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failure(error) => error.exit_code(),
        }
    }
}

fn main() {
    let raw_arguments = std::env::args_os().collect::<Vec<_>>();
    let diagnostic_hint = cli::diagnostic_format_hint(&raw_arguments);
    let parsed = match cli::Cli::try_parse_from(&raw_arguments) {
        Ok(parsed) => parsed,
        Err(error) => {
            render_clap_error(&error, diagnostic_hint);
            std::process::exit(error.exit_code());
        }
    };
    let diagnostic_format = parsed.diagnostic_format;
    let result = dispatch_cli(parsed);
    let (result, program_exit_code) = match result {
        Ok(program_exit_code) => (Ok(()), program_exit_code),
        Err(error) => (Err(error), None),
    };
    let outcome = CommandOutcome::from_result(result);
    render_cli_outcome(&outcome, diagnostic_format);
    if outcome.exit_code() != 0 {
        std::process::exit(outcome.exit_code());
    }
    if let Some(exit_code) = program_exit_code
        && exit_code != 0
    {
        std::process::exit(exit_code);
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch_cli(parsed: cli::Cli) -> CliResult<Option<i32>> {
    let format = parsed.diagnostic_format;
    let stdin_is_terminal = std::io::stdin().is_terminal();
    let Some(command) = parsed.command else {
        let mut invocation = parsed.script_and_args.into_iter();
        if let Some(path) = invocation.next() {
            let options = standalone::RunOptions {
                input: standalone::RunInput::Path(path.into()),
                program_arguments: invocation
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
                fuel: parsed.fuel.unwrap_or(standalone::DEFAULT_FUEL),
                limits_file: parsed.limits_file,
            };
            return run_with_options(&options, format).map(Some);
        }
        if parsed.limits_file.is_some() {
            return Err(CliError::usage(
                "`--limits-file` requires a script path; use `nexa repl` for REPL limits",
            ));
        }
        let options = repl::ReplOptions {
            limits: repl::ReplLimits {
                cell_fuel: parsed.fuel.unwrap_or(20_000),
                ..repl::ReplLimits::default()
            },
            prompt: stdin_is_terminal,
        };
        return repl_with_options(options, format).map(|()| None);
    };

    match command {
        cli::Command::Check(arguments) => check_command(arguments, format).map(|()| None),
        cli::Command::Build(arguments) => build_command(arguments, format).map(|()| None),
        cli::Command::Test(arguments) => test_command(arguments, format).map(|()| None),
        cli::Command::Lock(arguments) => lock_command(arguments, format).map(|()| None),
        cli::Command::Run(arguments) => {
            let mut path_and_args = arguments.path_and_args.into_iter();
            let (input, program_arguments) = match (arguments.project, arguments.package) {
                (None, None) => {
                    let path = path_and_args.next().ok_or_else(|| {
                        CliError::usage("`nexa run` requires PATH or `--project ... --package ...`")
                    })?;
                    (
                        standalone::RunInput::Path(path.into()),
                        path_and_args
                            .map(|argument| argument.to_string_lossy().into_owned())
                            .collect(),
                    )
                }
                (Some(configuration), Some(package_id)) => (
                    standalone::RunInput::Project {
                        configuration,
                        package_id,
                    },
                    path_and_args
                        .map(|argument| argument.to_string_lossy().into_owned())
                        .collect(),
                ),
                _ => {
                    return Err(CliError::usage(
                        "run input must be PATH or a paired `--project`/`--package` selection",
                    ));
                }
            };
            run_with_options(
                &standalone::RunOptions {
                    input,
                    program_arguments,
                    fuel: arguments.fuel,
                    limits_file: arguments.limits_file,
                },
                format,
            )
            .map(Some)
        }
        cli::Command::Repl(arguments) => repl_with_options(
            repl::ReplOptions {
                limits: repl::ReplLimits {
                    heap_objects: arguments.heap_objects,
                    cell_fuel: arguments.fuel,
                    committed_cells: arguments.max_cells,
                    diagnostic_history: arguments.history,
                    output_bytes: arguments.max_output_bytes,
                },
                prompt: !arguments.no_prompt && stdin_is_terminal,
            },
            format,
        )
        .map(|()| None),
        cli::Command::Exec(arguments) => exec_with_options(
            standalone::ExecOptions {
                module: arguments.module,
                function: arguments.function,
                runtime_arguments: arguments.arguments,
                fuel: arguments.fuel,
                limits_file: arguments.limits_file,
                trace_output: arguments.trace_output,
            },
            format,
            arguments.trace,
        )
        .map(|()| None),
        cli::Command::Dev(arguments) => {
            dev::dev_command(&arguments.project, arguments.once, format).map(|()| None)
        }
        cli::Command::Compile { file } => compile_file(&file, format).map(|()| None),
        cli::Command::Dump(arguments) => legacy_result(dump_module(arguments)).map(|()| None),
        cli::Command::Lsp => legacy_result(lsp::run()).map(|()| None),
        cli::Command::Migrate {
            command: cli::MigrateCommand::Check(arguments),
        } => legacy_result(migrate_check(arguments)).map(|()| None),
        cli::Command::Nidl {
            command: cli::NidlCommand::Check { file },
        } => legacy_result(check_nidl(&file)).map(|()| None),
        cli::Command::Nidl {
            command: cli::NidlCommand::Generate { file },
        } => legacy_result(generate_nidl(&file)).map(|()| None),
        cli::Command::Qa { command } => match command {
            cli::QaCommand::Models => legacy_result(check_models()).map(|()| None),
            cli::QaCommand::ModelReplay { artifact } => {
                legacy_result(model_replay(&artifact)).map(|()| None)
            }
            cli::QaCommand::Corpus { format } => legacy_result(diagnostic_corpus_check(matches!(
                format,
                cli::CorpusFormat::Json
            )))
            .map(|()| None),
            cli::QaCommand::Fixtures { input } => legacy_result(fixture_check(
                &[input.to_string_lossy().into_owned()],
                format,
            ))
            .map(|()| None),
            cli::QaCommand::Baseline => legacy_result(check_baseline()).map(|()| None),
            cli::QaCommand::Machines => legacy_result(check_machines()).map(|()| None),
            cli::QaCommand::Verify(arguments) => {
                legacy_result(verify_command(arguments, format)).map(|()| None)
            }
        },
    }
}

fn render_clap_error(error: &clap::Error, format: DiagnosticFormat) {
    if error.exit_code() == 0 || format == DiagnosticFormat::Human {
        let _ = error.print();
        return;
    }
    let envelope = json!({
        "status": "error",
        "exitCode": error.exit_code(),
        "message": error.to_string(),
    });
    eprintln!(
        "{}",
        serde_json::to_string(&envelope).expect("clap error JSON serialization does not fail")
    );
}

fn render_cli_outcome(outcome: &CommandOutcome, diagnostic_format: DiagnosticFormat) {
    let CommandOutcome::Failure(error) = outcome else {
        return;
    };
    if error.already_rendered {
        return;
    }
    match diagnostic_format {
        DiagnosticFormat::Human => eprintln!("nexa: {error}"),
        DiagnosticFormat::Json => eprintln!(
            "{}",
            serde_json::to_string(&json!({
                "status": "error",
                "message": error.message,
                "exitCode": error.exit_code(),
            }))
            .expect("diagnostic JSON serialization does not fail")
        ),
        DiagnosticFormat::Ndjson => eprintln!(
            "{}",
            serde_json::to_string(&json!({
                "schema": 1,
                "status": "error",
                "message": error.message,
                "exitCode": error.exit_code(),
            }))
            .expect("diagnostic JSON serialization does not fail")
        ),
    }
}

fn legacy_result(result: Result<(), String>) -> CliResult<()> {
    result.map_err(classify_legacy_error)
}

fn classify_legacy_error(error: String) -> CliError {
    let lower = error.to_ascii_lowercase();
    if lower.starts_with("usage:")
        || lower.starts_with("unknown ")
        || lower.starts_with("unexpected ")
        || lower.contains("missing value for")
    {
        CliError::usage(error)
    } else if lower.contains("could not read")
        || lower.contains("could not write")
        || lower.contains("could not resolve")
        || lower.contains("worker")
        || lower.contains("lsp ")
    {
        CliError::internal(error)
    } else {
        CliError::diagnostic(error)
    }
}

fn diagnostic_corpus_check(json_output: bool) -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let engine = nexa_embed::run_engine_diagnostic_cases(&root)?;
    let report = nexa::run_diagnostic_corpus(&root, engine)?;
    if !report.missing_codes.is_empty()
        || !report.unexpected_codes.is_empty()
        || report.source_backed_inexact_spans != 0
        || !report.case_format.invalid_pipelines.is_empty()
        || report.engine.registered != report.engine.observed_through_real_paths
        || report.engine.direct_diagnostic_construction != 0
    {
        return Err("diagnostic corpus contains failed cases".into());
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "diagnostic corpus: {} registered, {} observed, {} deterministic",
            report.registered_codes, report.observed_codes, report.deterministic_cases
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn check_command(arguments: cli::CheckArgs, format: DiagnosticFormat) -> CliResult<()> {
    let cli::CheckArgs {
        input,
        project: project_path,
        contract,
        policy,
        manifest_only,
        limits_file,
    } = arguments;
    if let Some(project_path) = project_path {
        let project = project::LoadedProject::load(&project_path)?;
        let builds = project.resolved_builds(true)?;
        let mut modules = 0_usize;
        let mut session = nexa::PackageBuildSession::new();
        for build in &builds {
            let compiled = compile_resolved_build_with_session(
                &mut session,
                build,
                1,
                None,
                &project.required_entrypoints,
                false,
                format,
            )?;
            modules = modules.saturating_add(compiled.module_count);
        }
        print_success(
            format,
            "check",
            &json!({
                "project": project.config_path,
                "packages": builds.len(),
                "modules": modules,
                "validationLevel": "full-policy",
            }),
            &format!(
                "checked {} packages ({} Modules) from {}",
                builds.len(),
                modules,
                project.config_path.display()
            ),
        );
        return Ok(());
    }
    let input =
        input.ok_or_else(|| CliError::usage("`nexa check` requires INPUT or `--project`"))?;
    if input.is_dir() {
        if limits_file.is_some() {
            return Err(CliError::usage(
                "`--limits-file` is only valid for a single source input",
            ));
        }
        let package = PackageCheckArguments {
            directory: input,
            contract,
            policy,
            manifest_only,
        };
        if package.manifest_only {
            let manifest_path = package.directory.join("package.toml");
            let manifest_source = std::fs::read_to_string(&manifest_path).map_err(|error| {
                CliError::internal(format!(
                    "could not read {}: {error}",
                    manifest_path.display()
                ))
            })?;
            let manifest =
                nexa_analysis::PackageManifest::parse(&manifest_source).map_err(|error| {
                    CliError::diagnostic(format!("invalid {}: {error}", manifest_path.display()))
                })?;
            print_success(
                format,
                "check",
                &json!({
                    "package": package.directory,
                    "packageId": manifest.id.as_str(),
                    "validationLevel": "manifest-only",
                }),
                &format!("checked package manifest {}", package.directory.display()),
            );
            return Ok(());
        }
        let contract = package.contract.as_ref().ok_or_else(|| {
            CliError::usage("Package check requires `--contract` or `--manifest-only`")
        })?;
        let contract_source = std::fs::read_to_string(contract).map_err(|error| {
            CliError::internal(format!("could not read {}: {error}", contract.display()))
        })?;
        let contract_model = nexa::parse_nidl(&contract_source).map_err(|error| {
            CliError::diagnostic(format!("invalid {}: {error}", contract.display()))
        })?;
        let host_contract = project::HostContractSnapshot::with_source(
            &contract_model,
            nexa::SourceIdentity::standalone(contract.to_string_lossy().into_owned()),
            Arc::<str>::from(contract_source),
        )?;
        let (source_id, policy, validation_level) = if let Some(policy_path) = &package.policy {
            let (source_id, policy) = project::load_policy(policy_path)?;
            (source_id, Some(policy), "full-policy")
        } else {
            (
                nexa_analysis::SourceId::new("contract-check")
                    .map_err(|error| CliError::internal(error.to_string()))?,
                None,
                "contract",
            )
        };
        let build = project::resolve_direct_package(
            &package.directory,
            source_id,
            policy.as_ref(),
            &host_contract,
            true,
        )?;
        let compiled = compile_resolved_build(
            &build,
            1,
            None,
            build.host_contract.required_entrypoints.as_ref(),
            false,
            format,
        )?;
        let functions = compiled.function_count();
        print_success(
            format,
            "check",
            &json!({
                "package": package.directory,
                "packageId": build.package_id().as_str(),
                "contract": contract,
                "functions": functions,
                "modules": compiled.module_count,
                "buildFingerprint": compiled.identity.build_fingerprint,
                "validationLevel": validation_level,
            }),
            &functions.map_or_else(
                || {
                    format!(
                        "checked Library Package {}: {} Modules",
                        build.package_id(),
                        compiled.module_count
                    )
                },
                |functions| {
                    format!(
                        "checked Application Package {}: {} Modules, {} functions",
                        build.package_id(),
                        compiled.module_count,
                        functions
                    )
                },
            ),
        );
        return Ok(());
    }
    let path = input;
    let limits = load_verifier_limits(limits_file.as_deref()).map_err(classify_legacy_error)?;
    let source = std::fs::read_to_string(&path).map_err(|error| {
        CliError::internal(format!("could not read {}: {error}", path.display()))
    })?;
    let build = project::virtual_snippet(&source, &path)?;
    let compiled = compile_resolved_build_with_limits(
        &build,
        1,
        None,
        build.host_contract.required_entrypoints.as_ref(),
        false,
        limits,
        format,
    )?;
    let artifact = compiled_product(&compiled)?;
    print_success(
        format,
        "check",
        &json!({
            "source": path,
            "packageId": "nexa.snippet",
            "module": "main",
            "functions": artifact.module().functions.len(),
            "buildFingerprint": compiled.identity.build_fingerprint,
        }),
        &format!(
            "checked {} as nexa.snippet::main: {} functions",
            path.display(),
            artifact.module().functions.len()
        ),
    );
    Ok(())
}

struct PackageCheckArguments {
    directory: PathBuf,
    contract: Option<PathBuf>,
    policy: Option<PathBuf>,
    manifest_only: bool,
}

/// Renders a batch in the rustc-style human layout, with ANSI colors only when stderr is a TTY
/// and `NO_COLOR` is unset.
pub(crate) fn render_human_batch(batch: &nexa::DiagnosticBatch) -> String {
    use std::io::IsTerminal;
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if color {
        nexa::LeafDiagnosticRenderer::human_colored(batch)
    } else {
        nexa::LeafDiagnosticRenderer::human(batch)
    }
}

fn render_diagnostic_batch(
    batch: &nexa::DiagnosticBatch,
    format: DiagnosticFormat,
) -> CliResult<()> {
    let rendered = match format {
        DiagnosticFormat::Human => render_human_batch(batch),
        DiagnosticFormat::Json => nexa::LeafDiagnosticRenderer::json(batch)
            .map_err(|error| CliError::internal(error.to_string()))?,
        DiagnosticFormat::Ndjson => nexa::LeafDiagnosticRenderer::ndjson(batch)
            .map_err(|error| CliError::internal(error.to_string()))?,
    };
    eprintln!("{rendered}");
    Ok(())
}

fn diagnostics_for_build(
    build: &project::ResolvedBuild,
    batch: &nexa::DiagnosticBatch,
) -> CliResult<nexa::DiagnosticBatch> {
    let Some(origin) = &build.virtual_source_origin else {
        return Ok(batch.clone());
    };
    if !origin.source_text_is_original {
        return Err(CliError::internal(
            "virtual diagnostic origin does not preserve the original source text",
        ));
    }
    let unit = build
        .input
        .root_source_set
        .get(&origin.source_key)
        .ok_or_else(|| {
            CliError::internal(
                "virtual diagnostic origin is absent from the resolved source snapshot",
            )
        })?;
    if unit.role != nexa_analysis::SourceRole::Production
        || unit.virtual_module_path().is_none()
        || unit.text.as_ref() != origin.original_text.as_ref()
    {
        return Err(CliError::internal(
            "virtual diagnostic origin disagrees with the resolved source authority",
        ));
    }
    let internal = nexa::SourceIdentity::package(
        origin.source_key.package_id.as_str(),
        origin.source_key.path.as_str(),
    );
    let internal_snapshot = batch.sources().get(&internal).ok_or_else(|| {
        CliError::internal(
            "virtual diagnostic source is absent from the diagnostic source registry",
        )
    })?;
    if internal_snapshot.text() != unit.text.as_ref() {
        return Err(CliError::internal(
            "virtual diagnostic source bytes disagree with the resolved source authority",
        ));
    }
    let mut sources = nexa::SourceSnapshotRegistry::builder();
    for (identity, snapshot) in batch.sources().iter() {
        if identity == &internal {
            sources
                .insert(
                    origin.display_identity.clone(),
                    Arc::clone(&origin.original_text),
                )
                .map_err(|error| CliError::internal(error.to_string()))?;
        } else {
            sources
                .insert(identity.clone(), Arc::<str>::from(snapshot.text()))
                .map_err(|error| CliError::internal(error.to_string()))?;
        }
    }
    let mut remapped = nexa::DiagnosticBatch::with_default_limits(sources.build());
    remapped.inherit_suppressed(batch);
    for diagnostic in batch.diagnostics() {
        let mut diagnostic = diagnostic.clone();
        for label in &mut diagnostic.labels {
            if label.source == internal {
                label.source = origin.display_identity.clone();
            }
        }
        for related in &mut diagnostic.related {
            if related.source == internal {
                related.source = origin.display_identity.clone();
            }
        }
        for fix in &mut diagnostic.fixes {
            if fix.source.as_ref() == Some(&internal) {
                fix.source = Some(origin.display_identity.clone());
            }
        }
        remapped.push(diagnostic);
    }
    Ok(remapped)
}

fn classify_facade_build_error(error: nexa::PackageBuildError) -> CliError {
    match error {
        error @ (nexa::PackageBuildError::Compile(_)
        | nexa::PackageBuildError::Environment(_)
        | nexa::PackageBuildError::Verify(_)
        | nexa::PackageBuildError::MissingRequiredEntrypoint(_)
        | nexa::PackageBuildError::EntrypointSignatureMismatch { .. }) => {
            CliError::diagnostic(error.to_string())
        }
        error => CliError::internal(error.to_string()),
    }
}

fn compile_resolved_build(
    build: &project::ResolvedBuild,
    generation: u64,
    contract: Option<&nexa::ValidatedContract>,
    required_entrypoints: &[String],
    include_tests: bool,
    format: DiagnosticFormat,
) -> CliResult<project::CompiledBuild> {
    finish_resolved_build(
        build,
        build.compile(generation, contract, required_entrypoints, include_tests),
        format,
    )
}

#[allow(clippy::too_many_arguments)]
fn compile_resolved_build_with_limits(
    build: &project::ResolvedBuild,
    generation: u64,
    contract: Option<&nexa::ValidatedContract>,
    required_entrypoints: &[String],
    include_tests: bool,
    verifier_limits: nexa::VerifierLimits,
    format: DiagnosticFormat,
) -> CliResult<project::CompiledBuild> {
    finish_resolved_build(
        build,
        build.compile_with_limits(
            generation,
            contract,
            required_entrypoints,
            include_tests,
            verifier_limits,
        ),
        format,
    )
}

fn compile_resolved_build_with_session(
    session: &mut nexa::PackageBuildSession,
    build: &project::ResolvedBuild,
    generation: u64,
    contract: Option<&nexa::ValidatedContract>,
    required_entrypoints: &[String],
    include_tests: bool,
    format: DiagnosticFormat,
) -> CliResult<project::CompiledBuild> {
    finish_resolved_build(
        build,
        build.compile_with_session(
            session,
            generation,
            contract,
            required_entrypoints,
            include_tests,
        ),
        format,
    )
}

#[allow(clippy::too_many_arguments)]
fn compile_resolved_build_with_session_and_limits(
    session: &mut nexa::PackageBuildSession,
    build: &project::ResolvedBuild,
    generation: u64,
    contract: Option<&nexa::ValidatedContract>,
    required_entrypoints: &[String],
    include_tests: bool,
    verifier_limits: nexa::VerifierLimits,
    format: DiagnosticFormat,
) -> CliResult<project::CompiledBuild> {
    finish_resolved_build(
        build,
        build.compile_with_session_and_limits(
            session,
            generation,
            contract,
            required_entrypoints,
            include_tests,
            verifier_limits,
        ),
        format,
    )
}

fn finish_resolved_build(
    build: &project::ResolvedBuild,
    result: Result<project::CompiledBuild, project::BuildCompileError>,
    format: DiagnosticFormat,
) -> CliResult<project::CompiledBuild> {
    match result {
        Ok(compiled) => Ok(compiled),
        Err(project::BuildCompileError::Cli(error)) => Err(error),
        Err(project::BuildCompileError::Facade(nexa::PackageBuildError::AnalysisFailed(batch))) => {
            let batch = diagnostics_for_build(build, &batch)?;
            render_diagnostic_batch(&batch, format)?;
            Err(CliError::rendered_diagnostic("Package analysis failed"))
        }
        Err(project::BuildCompileError::Facade(nexa::PackageBuildError::CompileFailed(batch))) => {
            let batch = diagnostics_for_build(build, &batch)?;
            render_diagnostic_batch(&batch, format)?;
            Err(CliError::rendered_diagnostic("Package compilation failed"))
        }
        Err(project::BuildCompileError::Facade(error)) => Err(classify_facade_build_error(error)),
    }
}

fn compiled_product(
    compiled: &project::CompiledBuild,
) -> CliResult<&nexa::CompiledPackageArtifact> {
    compiled.product().ok_or_else(|| {
        CliError::internal("Application compilation did not return a product artifact")
    })
}

#[allow(clippy::too_many_lines)]
fn build_command(arguments: cli::BuildArgs, format: DiagnosticFormat) -> CliResult<()> {
    let cli::BuildArgs {
        input: source,
        project: project_path,
        contract,
        output,
        limits_file,
        dump_source_map,
    } = arguments;
    if output
        .as_ref()
        .is_some_and(|path| path.file_name().is_some_and(|name| name == "nexa.lock"))
    {
        return Err(CliError::usage(
            "only `nexa lock` may write a file named `nexa.lock`",
        ));
    }
    let limits = load_verifier_limits(limits_file.as_deref()).map_err(classify_legacy_error)?;

    if let Some(project_path) = project_path {
        let project = project::LoadedProject::load(&project_path)?;
        let builds = project.resolved_builds(true)?;
        let application_builds = builds
            .iter()
            .filter(|build| build.root.manifest.is_application())
            .collect::<Vec<_>>();
        if application_builds.len() > 1
            && output
                .as_ref()
                .is_some_and(|path| path.extension().is_some_and(|extension| extension == "nxb"))
        {
            return Err(CliError::usage(
                "project build output must be a directory when multiple Applications are present",
            ));
        }
        let output_root = output.unwrap_or_else(|| project.root.join("target/nexa"));
        if application_builds.len() != 1 || output_root.extension().is_none() {
            std::fs::create_dir_all(&output_root).map_err(|error| {
                CliError::internal(format!(
                    "could not create build output {}: {error}",
                    output_root.display()
                ))
            })?;
        }
        let mut outputs = Vec::new();
        let mut session = nexa::PackageBuildSession::new();
        for build in application_builds {
            let compiled = compile_resolved_build_with_session_and_limits(
                &mut session,
                build,
                1,
                None,
                &project.required_entrypoints,
                false,
                limits,
                format,
            )?;
            let artifact = compiled_product(&compiled)?;
            let destination = if output_root.extension().is_some() {
                output_root.clone()
            } else {
                output_root.join(format!("{}.nxb", build.package_id()))
            };
            write_bytecode(&destination, artifact)?;
            outputs.push(destination);
            if dump_source_map {
                let mut rendered = String::new();
                render_source_map(&mut rendered, artifact.module());
                print!("{rendered}");
            }
        }
        print_success(
            format,
            "build",
            &json!({
                "project": project.config_path,
                "outputs": outputs,
                "packages": outputs.len(),
            }),
            &format!("built {} project Applications", outputs.len()),
        );
        return Ok(());
    }

    let source = source
        .ok_or_else(|| CliError::usage("usage: nexa build <source-or-package> [-o module.nxb]"))?;
    if source.is_dir() {
        let contract = contract
            .ok_or_else(|| CliError::usage("Package build requires `--contract <app_api.nidl>`"))?;
        let contract_source = std::fs::read_to_string(&contract).map_err(|error| {
            CliError::internal(format!("could not read {}: {error}", contract.display()))
        })?;
        let contract_model = nexa::parse_nidl(&contract_source).map_err(|error| {
            CliError::diagnostic(format!("invalid {}: {error}", contract.display()))
        })?;
        let host_contract = project::HostContractSnapshot::with_source(
            &contract_model,
            nexa::SourceIdentity::standalone(contract.to_string_lossy().into_owned()),
            Arc::<str>::from(contract_source),
        )?;
        let source_id = nexa_analysis::SourceId::new("contract-build")
            .map_err(|error| CliError::internal(error.to_string()))?;
        let build =
            project::resolve_direct_package(&source, source_id, None, &host_contract, true)?;
        if !build.root.manifest.is_application() {
            return Err(CliError::usage(
                "`nexa build` accepts only Application Packages",
            ));
        }
        let compiled = compile_resolved_build_with_limits(
            &build,
            1,
            None,
            build.host_contract.required_entrypoints.as_ref(),
            false,
            limits,
            format,
        )?;
        let artifact = compiled_product(&compiled)?;
        let output = output.unwrap_or_else(|| {
            source.join(format!(
                "{}.nxb",
                build.package_id().as_str().replace('.', "-")
            ))
        });
        write_bytecode(&output, artifact)?;
        if dump_source_map {
            let mut rendered = String::new();
            render_source_map(&mut rendered, artifact.module());
            print!("{rendered}");
        }
        print_success(
            format,
            "build",
            &json!({
                "package": source,
                "packageId": build.package_id().as_str(),
                "output": output,
                "modules": compiled.module_count,
                "buildFingerprint": compiled.identity.build_fingerprint,
            }),
            &format!(
                "built Package {} -> {}",
                build.package_id(),
                output.display()
            ),
        );
        return Ok(());
    }
    if contract.is_some() {
        return Err(CliError::usage(
            "`--contract` is accepted only for Package-directory builds",
        ));
    }
    let source_text = std::fs::read_to_string(&source).map_err(|error| {
        CliError::internal(format!("could not read {}: {error}", source.display()))
    })?;
    let build = project::virtual_snippet(&source_text, &source)?;
    let compiled = compile_resolved_build_with_limits(
        &build,
        1,
        None,
        build.host_contract.required_entrypoints.as_ref(),
        false,
        limits,
        format,
    )?;
    let artifact = compiled_product(&compiled)?;
    let output = output.unwrap_or_else(|| source.with_extension("nxb"));
    write_bytecode(&output, artifact)?;
    if dump_source_map {
        let mut rendered = String::new();
        render_source_map(&mut rendered, artifact.module());
        print!("{rendered}");
    }
    print_success(
        format,
        "build",
        &json!({
            "source": source,
            "packageId": "nexa.snippet",
            "module": "main",
            "output": output,
            "buildFingerprint": compiled.identity.build_fingerprint,
        }),
        &format!(
            "built {} as nexa.snippet::main -> {}",
            source.display(),
            output.display()
        ),
    );
    Ok(())
}

fn write_bytecode(path: &Path, artifact: &nexa::CompiledPackageArtifact) -> CliResult<()> {
    if path.file_name().is_some_and(|name| name == "nexa.lock") {
        return Err(CliError::usage(
            "only `nexa lock` may write a file named `nexa.lock`",
        ));
    }
    std::fs::write(path, artifact.encode_module())
        .map_err(|error| CliError::internal(format!("could not write {}: {error}", path.display())))
}

fn lock_command(arguments: cli::LockArgs, format: DiagnosticFormat) -> CliResult<()> {
    let project_mode = arguments.project.is_some();
    let builds = match (arguments.input, arguments.project) {
        (None, Some(path)) => {
            let project = project::LoadedProject::load(&path)?;
            let mut builds = Vec::new();
            for package in project.package_directories()? {
                if package.directory.join("package.toml").is_file() {
                    builds.push(project.resolve_package_for_lock(&package)?);
                }
            }
            builds
        }
        (Some(directory), None) => {
            let source_id = nexa_analysis::SourceId::new("cli-lock")
                .map_err(|error| CliError::internal(error.to_string()))?;
            vec![project::resolve_direct_package_for_lock(
                &directory, source_id,
            )?]
        }
        _ => {
            return Err(CliError::usage(
                "usage: nexa lock <package-directory> | nexa lock --project <nexa.dev.toml>",
            ));
        }
    };

    let mut written = Vec::new();
    for build in builds {
        if project_mode && build.root.manifest.dependencies.is_empty() {
            continue;
        }
        let lock_path = build.root.directory.join("nexa.lock");
        std::fs::write(&lock_path, build.canonical_lock.canonical_bytes()).map_err(|error| {
            CliError::internal(format!("could not write {}: {error}", lock_path.display()))
        })?;
        written.push(json!({
            "packageId": build.package_id().as_str(),
            "path": lock_path,
            "packages": build.dependency_graph.packages.len(),
            "edges": build.dependency_graph.edges.len(),
        }));
    }
    let written_count = written.len();
    print_success(
        format,
        "lock",
        &json!({
            "locks": written,
            "written": written_count,
        }),
        &format!("wrote {written_count} canonical nexa.lock file(s)"),
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn test_command(arguments: cli::TestArgs, format: DiagnosticFormat) -> CliResult<()> {
    let cli::TestArgs {
        input: directory,
        project: project_path,
        contract,
        fuel,
    } = arguments;
    if project_path.is_none() && directory.is_none() {
        return Err(CliError::usage(
            "usage: nexa test <package-directory> --contract <app_api.nidl> | \
             nexa test --project <nexa.dev.toml>",
        ));
    }

    let mut results = Vec::new();
    if let Some(project_path) = project_path {
        let project = project::LoadedProject::load(&project_path)?;
        let mut session = nexa::PackageBuildSession::new();
        for build in project.resolved_builds(true)? {
            let compiled = compile_resolved_build_with_session(
                &mut session,
                &build,
                1,
                None,
                &project.required_entrypoints,
                true,
                format,
            )?;
            results.extend(run_compiled_tests(&compiled, fuel)?);
        }
    } else {
        let directory = directory.expect("exclusive target was checked");
        let contract = contract
            .ok_or_else(|| CliError::usage("Package test requires `--contract <app_api.nidl>`"))?;
        let contract_source = std::fs::read_to_string(&contract).map_err(|error| {
            CliError::internal(format!("could not read {}: {error}", contract.display()))
        })?;
        let contract_model = nexa::parse_nidl(&contract_source).map_err(|error| {
            CliError::diagnostic(format!("invalid {}: {error}", contract.display()))
        })?;
        let host_contract = project::HostContractSnapshot::with_source(
            &contract_model,
            nexa::SourceIdentity::standalone(contract.to_string_lossy().into_owned()),
            Arc::<str>::from(contract_source),
        )?;
        let source_id = nexa_analysis::SourceId::new("contract-test")
            .map_err(|error| CliError::internal(error.to_string()))?;
        let build =
            project::resolve_direct_package(&directory, source_id, None, &host_contract, true)?;
        let compiled = compile_resolved_build(
            &build,
            1,
            None,
            build.host_contract.required_entrypoints.as_ref(),
            true,
            format,
        )?;
        results.extend(run_compiled_tests(&compiled, fuel)?);
    }
    results.sort_by(|left, right| {
        (
            left.package.as_str(),
            left.module.as_str(),
            left.name.as_str(),
            &left.span,
        )
            .cmp(&(
                right.package.as_str(),
                right.module.as_str(),
                right.name.as_str(),
                &right.span,
            ))
    });
    let passed = results
        .iter()
        .filter(|result| result.status == nexa::TestStatus::Pass)
        .count();
    let failed = results
        .iter()
        .filter(|result| result.status == nexa::TestStatus::Fail)
        .count();
    let errors = results
        .iter()
        .filter(|result| result.status == nexa::TestStatus::Error)
        .count();
    render_test_results(&results, passed, failed, errors, format)?;
    if failed != 0 || errors != 0 {
        Err(CliError::rendered_diagnostic(format!(
            "{failed} Package test(s) failed and {errors} errored"
        )))
    } else {
        Ok(())
    }
}

fn run_compiled_tests(
    compiled: &project::CompiledBuild,
    fuel: u64,
) -> CliResult<Vec<nexa::TestResult>> {
    let artifact = compiled.tests().ok_or_else(|| {
        CliError::internal("Package test compilation did not return a test artifact")
    })?;
    artifact
        .run(nexa::PackageTestOptions { fuel_limit: fuel })
        .map(|run| run.results)
        .map_err(|error| match error {
            nexa::PackageTestRunError::InvalidOptions(message) => CliError::usage(message),
            other => CliError::diagnostic(format_package_test_error(&other)),
        })
}

fn format_package_test_error(error: &nexa::PackageTestRunError) -> String {
    match error {
        nexa::PackageTestRunError::InvalidDeclarations(declarations) => declarations
            .iter()
            .map(|declaration| {
                format!(
                    "{}::{}::{} at {}: {}",
                    declaration.package,
                    declaration.module,
                    declaration.name,
                    declaration.span,
                    declaration.reason
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        nexa::PackageTestRunError::Ineligible(violations) => violations
            .iter()
            .map(|violation| {
                let path = violation
                    .path
                    .iter()
                    .map(|function| {
                        format!(
                            "{}::{}::{} at {}",
                            function.package, function.module, function.name, function.span
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" -> ");
                format!(
                    "{}::{}::{} at {} reaches forbidden {:?} effect in \
                     {}::{}::{} at {}; call path: {path}",
                    violation.test.package,
                    violation.test.module,
                    violation.test.name,
                    violation.test.span,
                    violation.reason,
                    violation.function.package,
                    violation.function.module,
                    violation.function.name,
                    violation.function.span,
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn render_test_results(
    results: &[nexa::TestResult],
    passed: usize,
    failed: usize,
    errors: usize,
    format: DiagnosticFormat,
) -> CliResult<()> {
    let values = results.iter().map(test_result_json).collect::<Vec<_>>();
    match format {
        DiagnosticFormat::Human => {
            for result in results {
                println!(
                    "{} {} at {} (instructions={}, fuel={})",
                    result.status,
                    result.qualified_name(),
                    result.span,
                    result.instructions,
                    result.fuel
                );
                if let Some(error) = &result.error {
                    println!("  {error:?}");
                }
                for frame in &result.stack {
                    println!(
                        "  at {}::{}::{}{}",
                        frame.package,
                        frame.module,
                        frame.function,
                        frame
                            .span
                            .as_ref()
                            .map_or_else(String::new, |span| format!(" ({span})"))
                    );
                }
            }
            println!(
                "test result: {}. {} passed; {} failed; {} errors",
                if failed == 0 && errors == 0 {
                    "ok"
                } else {
                    "FAILED"
                },
                passed,
                failed,
                errors
            );
        }
        DiagnosticFormat::Json => println!(
            "{}",
            serde_json::to_string(&json!({
                "schema": 1,
                "command": "test",
                "status": if failed == 0 && errors == 0 { "ok" } else { "failed" },
                "results": values,
                "summary": {
                    "total": results.len(),
                    "passed": passed,
                    "failed": failed,
                    "errors": errors,
                },
            }))
            .map_err(|error| CliError::internal(error.to_string()))?
        ),
        DiagnosticFormat::Ndjson => {
            for value in values {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "schema": 1,
                        "type": "test-result",
                        "result": value,
                    }))
                    .map_err(|error| CliError::internal(error.to_string()))?
                );
            }
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "schema": 1,
                    "type": "test-summary",
                    "total": results.len(),
                    "passed": passed,
                    "failed": failed,
                    "errors": errors,
                    "status": if failed == 0 && errors == 0 { "ok" } else { "failed" },
                }))
                .map_err(|error| CliError::internal(error.to_string()))?
            );
        }
    }
    Ok(())
}

fn test_result_json(result: &nexa::TestResult) -> Value {
    json!({
        "package": result.package,
        "module": result.module,
        "name": result.name,
        "qualifiedName": result.qualified_name(),
        "source": result.span.source,
        "span": {"start": result.span.start, "end": result.span.end},
        "status": result.status.as_str(),
        "error": result.error.as_ref().map(|error| format!("{error:?}")),
        "stack": result.stack.iter().map(|frame| json!({
            "package": frame.package,
            "module": frame.module,
            "function": frame.function,
            "source": frame.span.as_ref().map(|span| span.source.clone()),
            "span": frame.span.as_ref().map(|span| json!({
                "start": span.start,
                "end": span.end,
            })),
        })).collect::<Vec<_>>(),
        "instructions": result.instructions,
        "fuel": result.fuel,
    })
}

fn verify_command(arguments: cli::VerifyArgs, format: DiagnosticFormat) -> Result<(), String> {
    let path = arguments.input;
    let limits = load_verifier_limits(arguments.limits_file.as_deref())?;
    verify_module_with_limits(&path, limits)?;
    print_success(
        format,
        "verify",
        &json!({"module": path}),
        &format!("verified {}", path.display()),
    );
    Ok(())
}

fn run_with_options(options: &standalone::RunOptions, format: DiagnosticFormat) -> CliResult<i32> {
    let verifier_limits =
        load_verifier_limits(options.limits_file.as_deref()).map_err(classify_legacy_error)?;
    let build = match &options.input {
        standalone::RunInput::Path(path) if path.is_file() => {
            if path.extension().is_none_or(|extension| extension != "nexa") {
                return Err(CliError::usage(
                    "`nexa run` accepts a `.nexa` source file or Package directory",
                ));
            }
            let source = std::fs::read_to_string(path).map_err(|error| {
                CliError::internal(format!("could not read {}: {error}", path.display()))
            })?;
            project::virtual_standalone_script(&source, path)?
        }
        standalone::RunInput::Path(path) if path.is_dir() => {
            let source_id = nexa_analysis::SourceId::new("standalone-cli")
                .map_err(|error| CliError::internal(error.to_string()))?;
            project::resolve_direct_standalone_package(path, source_id, None, true)?
        }
        standalone::RunInput::Path(path) => {
            return Err(CliError::environment(format!(
                "standalone input does not exist: {}",
                path.display()
            )));
        }
        standalone::RunInput::Project {
            configuration,
            package_id,
        } => {
            let project = project::LoadedProject::load(configuration)?;
            let packages = project.package_directories()?;
            let mut selected = None;
            for package in packages {
                let candidate = project::LoadedProject::resolve_standalone_package(&package, true)?;
                if candidate.package_id().as_str() == package_id {
                    selected = Some(candidate);
                    break;
                }
            }
            selected.ok_or_else(|| {
                CliError::diagnostic(format!(
                    "project {} does not contain Package `{package_id}`",
                    configuration.display()
                ))
            })?
        }
    };
    let mut session = nexa::PackageBuildSession::new();
    let compiled = finish_standalone_build(
        &build,
        build.compile_standalone_with_session_and_limits(&mut session, 1, verifier_limits),
        format,
    )?;
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    standalone::run_compiled(
        &compiled.artifact,
        &options.program_arguments,
        options.fuel,
        4_096,
        &cancelled,
    )
    .map_err(|error| match error {
        standalone::StandaloneRuntimeError::Trap(message) => CliError::runtime_trap(message),
        standalone::StandaloneRuntimeError::Internal(message) => CliError::internal(message),
    })
}

fn finish_standalone_build(
    build: &project::ResolvedBuild,
    result: Result<project::CompiledStandaloneBuild, project::BuildCompileError>,
    format: DiagnosticFormat,
) -> CliResult<project::CompiledStandaloneBuild> {
    match result {
        Ok(compiled) => Ok(compiled),
        Err(project::BuildCompileError::Cli(error)) => Err(error),
        Err(project::BuildCompileError::Facade(nexa::PackageBuildError::AnalysisFailed(batch))) => {
            let batch = diagnostics_for_build(build, &batch)?;
            render_diagnostic_batch(&batch, format)?;
            Err(CliError::rendered_diagnostic("Package analysis failed"))
        }
        Err(project::BuildCompileError::Facade(nexa::PackageBuildError::CompileFailed(batch))) => {
            let batch = diagnostics_for_build(build, &batch)?;
            render_diagnostic_batch(&batch, format)?;
            Err(CliError::rendered_diagnostic("Package compilation failed"))
        }
        Err(project::BuildCompileError::Facade(error)) => Err(classify_facade_build_error(error)),
    }
}

fn repl_with_options(options: repl::ReplOptions, format: DiagnosticFormat) -> CliResult<()> {
    let cancelled = repl::install_cancel_handler()?;
    let backend =
        repl::CanonicalReplBackend::new(options.limits, format).map_err(CliError::internal)?;
    repl::run(backend, options, cancelled.as_ref())
}

fn exec_with_options(
    options: standalone::ExecOptions,
    format: DiagnosticFormat,
    trace: bool,
) -> CliResult<()> {
    if options
        .module
        .extension()
        .is_none_or(|extension| extension != "nxb")
    {
        return Err(CliError::usage(
            "`nexa exec` accepts a bytecode `.nxb` module",
        ));
    }
    let limits =
        load_verifier_limits(options.limits_file.as_deref()).map_err(classify_legacy_error)?;
    let verified = load_verified(&options.module, limits, format)?;
    let runtime_arguments = options
        .runtime_arguments
        .iter()
        .copied()
        .map(nexa::prelude::RuntimeValue::I32)
        .collect::<Vec<_>>();
    let outcome = nexa::CheckedInterpreter::run(
        &verified,
        options.function,
        &runtime_arguments,
        options.fuel,
    )
    .map_err(|error| CliError::diagnostic(format!("execution failed: {error}")))?;
    let record = json!({
        "input": options.module,
        "function": options.function,
        "arguments": runtime_arguments.len(),
        "fuel_limit": options.fuel,
        "outcome": format!("{outcome:?}"),
    });
    if trace {
        let rendered = serde_json::to_string_pretty(&record)
            .map_err(|error| CliError::internal(format!("could not serialize trace: {error}")))?;
        if let Some(path) = options.trace_output {
            std::fs::write(&path, rendered).map_err(|error| {
                CliError::internal(format!("could not write {}: {error}", path.display()))
            })?;
        } else {
            println!("{rendered}");
        }
    } else {
        print_success(
            format,
            "exec",
            &record,
            &format!("exec completed: {outcome:?}"),
        );
    }
    Ok(())
}

fn fixture_check(arguments: &[String], format: DiagnosticFormat) -> Result<(), String> {
    let [path] = arguments else {
        return Err("usage: nexa fixture-check <fixture.json|directory>".into());
    };
    let path = Path::new(path);
    let paths = if path.is_dir() {
        files_with_extension(path, "json")?
    } else {
        vec![path.to_path_buf()]
    };
    if paths.is_empty() {
        return Err(format!("no JSON fixtures found under {}", path.display()));
    }
    for fixture in &paths {
        let bytes = std::fs::read(fixture)
            .map_err(|error| format!("could not read {}: {error}", fixture.display()))?;
        nexa_migrate::parse_state_fixture(&bytes, nexa_migrate::StateFixtureLimits::default())
            .map_err(|error| format!("{}: {error}", fixture.display()))?;
    }
    print_success(
        format,
        "fixture-check",
        &json!({"path": path, "fixtures": paths.len()}),
        &format!("validated {} migration fixtures", paths.len()),
    );
    Ok(())
}

fn model_replay(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let artifact: ModelFailureArtifact = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid model artifact: {error}"))?;
    if artifact.format_version != MODEL_FAILURE_ARTIFACT_VERSION {
        return Err(format!(
            "unsupported model artifact version {}",
            artifact.format_version
        ));
    }
    if artifact.path.last() != Some(&artifact.failure_event) {
        return Err("model artifact failure_event is not the final path event".into());
    }
    let model = artifact
        .model_config
        .get("model")
        .and_then(Value::as_str)
        .ok_or("model artifact has no model name")?;
    let visited = match model {
        "realm" => 4,
        name => {
            let (_, spec) = load_specs()?
                .into_iter()
                .find(|(_, spec)| spec.name == name)
                .ok_or_else(|| format!("unknown replay model `{name}`"))?;
            explore(&spec).visited_snapshots
        }
    };
    println!(
        "replayed {} against {model}: {} path events, {visited} explored states",
        path.display(),
        artifact.path.len()
    );
    Ok(())
}

fn load_verifier_limits(path: Option<&Path>) -> Result<nexa::VerifierLimits, String> {
    let Some(path) = path else {
        return Ok(nexa::VerifierLimits::default());
    };
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid limits file {}: {error}", path.display()))?;
    let number = |name: &str, fallback: u32| -> Result<u32, String> {
        value.get(name).map_or(Ok(fallback), |value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| format!("limits field `{name}` must be a u32"))
        })
    };
    let defaults = nexa::VerifierLimits::default();
    Ok(nexa::VerifierLimits {
        max_frame_bytes: number("max_frame_bytes", defaults.max_frame_bytes)?,
        max_immediate_cost: number("max_immediate_cost", defaults.max_immediate_cost)?,
        max_wcet_states: number("max_wcet_states", defaults.max_wcet_states)?,
    })
}

fn load_verified(
    path: &Path,
    limits: nexa::VerifierLimits,
    format: DiagnosticFormat,
) -> CliResult<nexa::VerifiedModule> {
    if path.extension().is_some_and(|extension| extension == "nxb") {
        let bytes = std::fs::read(path).map_err(|error| {
            CliError::internal(format!("could not read {}: {error}", path.display()))
        })?;
        let module = nexa::decode_module(&bytes, nexa::DecodeLimits::default())
            .map_err(|error| CliError::diagnostic(format!("bytecode decode failed: {error}")))?;
        nexa::verify_module(module, limits)
            .map_err(|error| CliError::diagnostic(format!("bytecode verification failed: {error}")))
    } else {
        let source = std::fs::read_to_string(path).map_err(|error| {
            CliError::internal(format!("could not read {}: {error}", path.display()))
        })?;
        let build = project::virtual_snippet(&source, path)?;
        let compiled = compile_resolved_build_with_limits(
            &build,
            1,
            None,
            build.host_contract.required_entrypoints.as_ref(),
            false,
            limits,
            format,
        )?;
        let artifact = compiled_product(&compiled)?;
        Ok(artifact.verified.clone())
    }
}

fn verify_with_limits(
    module: nexa::Module,
    limits: nexa::VerifierLimits,
) -> Result<nexa::VerifiedModule, String> {
    nexa::verify_module(module, limits)
        .map_err(|error| format!("bytecode verification failed: {error}"))
}

fn verify_module_with_limits(path: &Path, limits: nexa::VerifierLimits) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let module =
        nexa::Module::decode(&bytes).map_err(|error| format!("bytecode decode failed: {error}"))?;
    verify_with_limits(module, limits)?;
    Ok(())
}

fn print_success(format: DiagnosticFormat, command: &str, data: &Value, human: &str) {
    match format {
        DiagnosticFormat::Human => println!("{human}"),
        DiagnosticFormat::Json => println!(
            "{}",
            serde_json::to_string(&json!({
                "status": "ok",
                "command": command,
                "data": data,
            }))
            .expect("diagnostic JSON serialization does not fail")
        ),
        DiagnosticFormat::Ndjson => println!(
            "{}",
            serde_json::to_string(&json!({
                "schema": 1,
                "status": "ok",
                "command": command,
                "data": data,
            }))
            .expect("diagnostic JSON serialization does not fail")
        ),
    }
}

fn dump_module(arguments: cli::DumpArgs) -> Result<(), String> {
    let section = arguments
        .section
        .as_deref()
        .map(|section| {
            nexa::SectionKind::from_name(section)
                .ok_or_else(|| format!("unknown bytecode section `{section}`"))
        })
        .transpose()?;
    let path = arguments.module;
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let module =
        nexa::Module::decode(&bytes).map_err(|error| format!("bytecode decode failed: {error}"))?;
    let rendered = render_module_dump(&bytes, &module, section, arguments.dump_source_map)?;
    print!("{rendered}");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn render_module_dump(
    bytes: &[u8],
    module: &nexa::Module,
    selected: Option<nexa::SectionKind>,
    source_map_only: bool,
) -> Result<String, String> {
    use std::fmt::Write as _;

    let directory = nexa::Module::inspect_section_directory(bytes, nexa::DecodeLimits::default())
        .map_err(|error| format!("bytecode directory inspection failed: {error}"))?;
    let mut output = String::new();
    if source_map_only {
        render_source_map(&mut output, module);
        return Ok(output);
    }
    writeln!(
        output,
        "header magic=NXBC version={} sections={}",
        nexa::BYTECODE_VERSION,
        directory.len()
    )
    .expect("String writes do not fail");
    for entry in &directory {
        let name = nexa::SectionKind::ALL
            .into_iter()
            .find(|kind| *kind as u16 == entry.kind)
            .map_or("unknown", nexa::SectionKind::name);
        if selected.is_none_or(|kind| kind as u16 == entry.kind) {
            writeln!(
                output,
                "section {name} kind={} flags={} offset={} length={} count={} checksum={:08x}",
                entry.kind, entry.flags, entry.offset, entry.length, entry.count, entry.checksum
            )
            .expect("String writes do not fail");
        }
    }
    let render = |kind| selected.is_none_or(|selected| selected == kind);
    if render(nexa::SectionKind::Strings) {
        for (index, string) in module.strings.iter().enumerate() {
            writeln!(output, "string {index} {string:?}").expect("String writes do not fail");
        }
    }
    if render(nexa::SectionKind::Types) {
        let mut handles = module.state_handle_types.iter().collect::<Vec<_>>();
        handles.sort_by_key(|handle| handle.type_id);
        for handle in handles {
            writeln!(
                output,
                "state-handle {:016x} target={:?}",
                handle.type_id.0, handle.target
            )
            .expect("String writes do not fail");
        }
        let mut arrays = module.array_types.iter().collect::<Vec<_>>();
        arrays.sort_by_key(|array| array.type_id);
        for array in arrays {
            writeln!(
                output,
                "array {:016x} element={:?}",
                array.type_id.0, array.element
            )
            .expect("String writes do not fail");
        }
        let mut maps = module.map_types.iter().collect::<Vec<_>>();
        maps.sort_by_key(|map| map.type_id);
        for map in maps {
            writeln!(
                output,
                "map {:016x} key={:?} value={:?}",
                map.type_id.0, map.key, map.value
            )
            .expect("String writes do not fail");
        }
        let mut buffers = module.buffer_types.iter().collect::<Vec<_>>();
        buffers.sort_by_key(|buffer| buffer.type_id);
        for buffer in buffers {
            writeln!(
                output,
                "buffer {:016x} element={:?} ownership=vm-copy",
                buffer.type_id.0, buffer.element
            )
            .expect("String writes do not fail");
        }
        let mut snapshots = module.snapshot_types.iter().collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.type_id);
        for snapshot in snapshots {
            writeln!(
                output,
                "snapshot {:016x} content-type={:016x} ownership=host immutable=true",
                snapshot.type_id.0, snapshot.content_type.0
            )
            .expect("String writes do not fail");
        }
    }
    if render(nexa::SectionKind::Functions) {
        for (index, function) in module.functions.iter().enumerate() {
            writeln!(
                output,
                "function {index} effect={:?} registers={} frame_bytes={} signature={:?}",
                function.effect, function.registers, function.frame_bytes, function.signature
            )
            .expect("String writes do not fail");
        }
    }
    if render(nexa::SectionKind::Code) {
        for (function, body) in module.functions.iter().enumerate() {
            writeln!(output, "code function={function}").expect("String writes do not fail");
            for (pc, instruction) in body.code.iter().enumerate() {
                writeln!(output, "  {pc:06} {instruction:?}").expect("String writes do not fail");
            }
        }
    }
    if render(nexa::SectionKind::Enums) {
        let mut enums = module.enum_types.iter().collect::<Vec<_>>();
        enums.sort_by_key(|enum_type| enum_type.type_id);
        for enum_type in enums {
            writeln!(output, "enum {:016x}", enum_type.type_id.0)
                .expect("String writes do not fail");
            let mut variants = enum_type.variants.iter().collect::<Vec<_>>();
            variants.sort_by_key(|variant| (variant.tag, variant.stable_id));
            for variant in variants {
                writeln!(
                    output,
                    "  variant tag={} id={:016x} payload={:?}",
                    variant.tag, variant.stable_id.0, variant.payload_type
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa::SectionKind::Structs) {
        let mut structs = module.struct_types.iter().collect::<Vec<_>>();
        structs.sort_by_key(|struct_type| struct_type.type_id);
        for struct_type in structs {
            writeln!(output, "struct {:016x}", struct_type.type_id.0)
                .expect("String writes do not fail");
            for (index, field) in struct_type.fields.iter().enumerate() {
                writeln!(
                    output,
                    "  field index={index} id={:016x} type={:?}",
                    field.stable_id.0, field.ty,
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa::SectionKind::Classes) {
        let mut classes = module.class_types.iter().collect::<Vec<_>>();
        classes.sort_by_key(|class_type| class_type.type_id);
        for class_type in classes {
            writeln!(output, "class {:016x}", class_type.type_id.0)
                .expect("String writes do not fail");
            for (index, field) in class_type.fields.iter().enumerate() {
                writeln!(
                    output,
                    "  field index={index} id={:016x} type={:?} mutable=true",
                    field.stable_id.0, field.ty,
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa::SectionKind::StateSchemas) {
        let mut state_types = module.state_schema.types.iter().collect::<Vec<_>>();
        state_types.sort_by_key(|state_type| state_type.stable_id);
        for state_type in state_types {
            writeln!(
                output,
                "state-class {:016x} version={}",
                state_type.stable_id.0, state_type.version
            )
            .expect("String writes do not fail");
            let mut fields = state_type.fields.iter().collect::<Vec<_>>();
            fields.sort_by_key(|field| field.stable_id);
            for field in fields {
                writeln!(
                    output,
                    "  field id={:016x} type={:?} persistent=true",
                    field.stable_id.0, field.ty
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa::SectionKind::SourceMap) {
        render_source_map(&mut output, module);
    }
    Ok(output)
}

fn render_source_map(output: &mut String, module: &nexa::Module) {
    use std::fmt::Write as _;

    let mut entries = module.source_map.clone();
    entries.sort_by_key(|entry| {
        (
            entry.function,
            entry.pc_start,
            entry.pc_end,
            entry.span.file,
            entry.span.start,
            entry.span.end,
        )
    });
    for entry in entries {
        writeln!(
            output,
            "source-map function={} pc={}..{} file={} span={}..{}",
            entry.function,
            entry.pc_start,
            entry.pc_end,
            entry.span.file.0,
            entry.span.start,
            entry.span.end
        )
        .expect("String writes do not fail");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrateOutputFormat {
    Human,
    Json,
}

fn render_migrate_result(
    result: &nexa_migrate::MigrateCheckResult,
    format: MigrateOutputFormat,
) -> Result<String, String> {
    match format {
        MigrateOutputFormat::Human => {
            let mut rendered = format!(
                "migration check passed\n\
             old schema fingerprint: {}\n\
             new schema fingerprint: {}\n\
             migration entry: {}\n\
             migration hash: {:016x}\n\
             final state hash: {:016x}\n\
             objects: {}\n\
             objects read/created: {}/{}\n\
             fields written: {}\n\
             preserve/replace/delete: {}/{}/{}\n\
             generation changes: {}\n\
             handle remaps: {}\n\
             peak objects/fields/forwarding: {}/{}/{}\n\
             peak state bytes/GC roots: {}/{}\n\
             fuel: {}\n\
             call depth: {}\n",
                result.old_schema_fingerprint,
                result.new_schema_fingerprint,
                result.migration_entry,
                result.migration_hash,
                result.final_state_hash,
                result.final_object_count,
                result.usage.objects_read,
                result.usage.objects_created,
                result.usage.fields_written,
                result.usage.preserved,
                result.usage.replaced,
                result.usage.deleted,
                result.usage.generation_changes,
                result.usage.handle_remaps,
                result.usage.object_peak,
                result.usage.field_peak,
                result.usage.forwarding_peak,
                result.usage.payload_byte_peak,
                result.usage.gc_root_peak,
                result.usage.fuel_used,
                result.usage.max_call_depth_used,
            );
            if let Some(diff) = &result.state_diff {
                use std::fmt::Write as _;
                writeln!(
                    rendered,
                    "diff added/removed/changed: {}/{}/{}",
                    diff.added_objects.len(),
                    diff.removed_objects.len(),
                    diff.changed_objects.len()
                )
                .expect("String writes do not fail");
            }
            if let Some(state) = &result.output_state {
                rendered.push_str("state:\n");
                rendered.push_str(
                    &serde_json::to_string_pretty(state)
                        .map_err(|error| format!("could not serialize output state: {error}"))?,
                );
                rendered.push('\n');
            }
            Ok(rendered)
        }
        MigrateOutputFormat::Json => serde_json::to_string_pretty(result)
            .map_err(|error| format!("could not serialize migration result: {error}")),
    }
}

fn migrate_check(arguments: cli::MigrateCheckArgs) -> Result<(), String> {
    let mut config = nexa_migrate::MigrateCheckConfig {
        dump_state: arguments.dump_state,
        diff_state: arguments.diff_state,
        ..nexa_migrate::MigrateCheckConfig::default()
    };
    let limits = &mut config.migration_limits;
    if let Some(value) = arguments.max_objects {
        limits.max_objects = value;
    }
    if let Some(value) = arguments.max_fields {
        limits.max_fields = value;
    }
    if let Some(value) = arguments.max_forwarding_entries {
        limits.max_forwarding_entries = value;
    }
    if let Some(value) = arguments.max_state_bytes {
        limits.max_state_bytes = value;
    }
    if let Some(value) = arguments.max_gc_roots {
        limits.max_gc_roots = value;
    }
    if let Some(value) = arguments.max_fuel {
        limits.max_fuel = value;
    }
    if let Some(value) = arguments.max_call_depth {
        limits.max_call_depth = value;
    }
    let old_bytes = std::fs::read(&arguments.old_module)
        .map_err(|error| format!("could not read {}: {error}", arguments.old_module.display()))?;
    let new_bytes = std::fs::read(&arguments.new_module)
        .map_err(|error| format!("could not read {}: {error}", arguments.new_module.display()))?;
    let state_bytes = std::fs::read(&arguments.state)
        .map_err(|error| format!("could not read {}: {error}", arguments.state.display()))?;
    let result = nexa_migrate::run_migrate_check(&old_bytes, &new_bytes, &state_bytes, config)
        .map_err(|error| format!("migration check failed: {error}"))?;
    let format = match arguments.format {
        cli::MigrateFormat::Human => MigrateOutputFormat::Human,
        cli::MigrateFormat::Json => MigrateOutputFormat::Json,
    };
    let rendered = render_migrate_result(&result, format)?;
    if let Some(output) = arguments.output {
        std::fs::write(&output, rendered)
            .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn compile_file(path: &Path, format: DiagnosticFormat) -> CliResult<()> {
    let source = std::fs::read_to_string(path).map_err(|error| {
        CliError::internal(format!("could not read {}: {error}", path.display()))
    })?;
    let build = project::virtual_snippet(&source, path)?;
    let compiled = compile_resolved_build(
        &build,
        1,
        None,
        build.host_contract.required_entrypoints.as_ref(),
        false,
        format,
    )?;
    let artifact = compiled_product(&compiled)?;
    print_success(
        format,
        "compile",
        &json!({
            "source": path,
            "packageId": "nexa.snippet",
            "module": "main",
            "functions": artifact.module().functions.len(),
            "buildFingerprint": compiled.identity.build_fingerprint,
        }),
        &format!(
            "compiled and verified {} as nexa.snippet::main: {} functions",
            path.display(),
            artifact.module().functions.len()
        ),
    );
    Ok(())
}

fn check_nidl(path: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let contract = nexa::parse_nidl(&source).map_err(|error| error.to_string())?;
    println!(
        "NIDL {} is valid; contract fingerprint {}",
        path.display(),
        nexa::contract_fingerprint(&contract)
    );
    Ok(())
}

fn generate_nidl(path: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let contract = nexa::parse_nidl(&source).map_err(|error| error.to_string())?;
    let generated =
        nexa::prelude::generate_rust_bindings(&contract).map_err(|error| error.to_string())?;
    print!("{generated}");
    Ok(())
}

fn check_baseline() -> Result<(), String> {
    for required in REQUIRED_BASELINE {
        if !Path::new(required).is_file() {
            return Err(format!("required normative file `{required}` is missing"));
        }
    }

    let index = std::fs::read_to_string(REQUIRED_BASELINE[0])
        .map_err(|error| format!("could not read baseline index: {error}"))?;
    let normative_files = files_with_extension(Path::new("baseline"), "md")?;
    check_decision_index(&index)?;
    check_normative_files(&index, &normative_files)?;
    check_deferred_api_leaks(Path::new("crates"))?;
    check_toolchain_consistency()?;

    let decision_count = table_rows(&index, "## Active decisions").len();
    println!(
        "baseline snapshot is complete: {} files, {decision_count} active decisions",
        normative_files.len()
    );
    Ok(())
}

fn check_decision_index(index: &str) -> Result<(), String> {
    let mut decision_ids = BTreeSet::new();
    let active = table_rows(index, "## Active decisions");
    if active.is_empty() {
        return Err("baseline index contains no active decisions".into());
    }
    for row in &active {
        if row.len() != 5 {
            return Err(format!(
                "malformed active decision row `{}`",
                row.join(" | ")
            ));
        }
        let id = &row[0];
        if !is_decision_id(id) {
            return Err(format!("invalid decision ID `{id}`"));
        }
        if row[1] != "Active" {
            return Err(format!(
                "decision `{id}` has invalid active status `{}`",
                row[1]
            ));
        }
        if !decision_ids.insert(id.clone()) {
            return Err(format!("decision `{id}` appears more than once"));
        }
        // A normative location lists one or more comma-separated paths.
        for location in row[3].split(',') {
            let location = location.trim().trim_matches('`');
            let normative_path = if location == "this file" {
                PathBuf::from(REQUIRED_BASELINE[0])
            } else {
                Path::new("baseline").join(location)
            };
            if !normative_path.is_file() {
                return Err(format!(
                    "active decision `{id}` refers to missing normative path `{}`",
                    normative_path.display()
                ));
            }
        }
    }

    for row in table_rows(index, "## Deferred decisions") {
        if row.len() != 3 || row[1] != "Deferred" {
            return Err(format!(
                "malformed deferred decision row `{}`",
                row.join(" | ")
            ));
        }
    }
    for row in table_rows(index, "## Superseded decisions") {
        if row.len() != 3 || row[1] != "Superseded" {
            return Err(format!(
                "malformed superseded decision row `{}`",
                row.join(" | ")
            ));
        }
    }
    Ok(())
}

fn check_normative_files(index: &str, normative_files: &[PathBuf]) -> Result<(), String> {
    let superseded = table_rows(index, "## Superseded decisions")
        .into_iter()
        .map(|row| row[0].to_lowercase())
        .collect::<Vec<_>>();
    for path in normative_files {
        let contents = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read `{}`: {error}", path.display()))?;
        if !has_version_declaration(&contents) {
            return Err(format!(
                "normative file `{}` has no version declaration",
                path.display()
            ));
        }
        if contents.contains("rationale/history/") {
            return Err(format!(
                "normative file `{}` refers to historical rationale as a source",
                path.display()
            ));
        }
        if path != Path::new(REQUIRED_BASELINE[0]) {
            let normalized = contents.to_lowercase();
            if let Some(item) = superseded
                .iter()
                .find(|item| normalized.contains(item.as_str()))
            {
                return Err(format!(
                    "normative file `{}` refers to superseded item `{item}`",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn check_deferred_api_leaks(root: &Path) -> Result<(), String> {
    const DEFERRED_IDENTIFIERS: &[&str] = &[
        "dynamic",
        "dynamicvalue",
        "usergeneric",
        "userdefinedgeneric",
        "crossmodule",
        "reloadgroup",
        "readlease",
        "writelease",
        "compatibleabiadapter",
        "strictdeterminism",
        "untrustedbytecode",
        "securityverifier",
        "aot",
        "jit",
    ];
    for path in files_with_extension(root, "rs")? {
        let contents = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read `{}`: {error}", path.display()))?;
        for (line_index, source_line) in contents.lines().enumerate() {
            let code = strip_string_literals(source_line.split("//").next().unwrap_or_default());
            for token in
                code.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            {
                let normalized = token.replace('_', "").to_ascii_lowercase();
                if DEFERRED_IDENTIFIERS.contains(&normalized.as_str()) {
                    return Err(format!(
                        "deferred identifier `{token}` appears in {}:{}",
                        path.display(),
                        line_index + 1
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check_toolchain_consistency() -> Result<(), String> {
    let toolchain = std::fs::read_to_string("rust-toolchain.toml")
        .map_err(|error| format!("could not read rust-toolchain.toml: {error}"))?;
    let channel = toolchain
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("channel")
                .and_then(|value| value.split_once('='))
                .map(|(_, value)| value.trim().trim_matches('"'))
        })
        .ok_or("rust-toolchain.toml has no channel")?;
    let readme = std::fs::read_to_string("README.md")
        .map_err(|error| format!("could not read README.md: {error}"))?;
    if !readme.contains(channel) {
        return Err(format!(
            "README.md does not mention pinned Rust toolchain `{channel}`"
        ));
    }
    Ok(())
}

fn strip_string_literals(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            output.push(' ');
        } else if character == '"' {
            in_string = true;
            output.push(' ');
        } else {
            output.push(character);
        }
    }
    output
}

fn files_with_extension(root: &Path, expected_extension: &str) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| format!("could not read {}: {error}", directory.display()))?
        {
            let path = entry
                .map_err(|error| format!("could not inspect {}: {error}", directory.display()))?
                .path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == expected_extension)
            {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn table_rows(contents: &str, heading: &str) -> Vec<Vec<String>> {
    let mut in_section = false;
    let mut rows = Vec::new();
    for line in contents.lines() {
        if line == heading {
            in_section = true;
            continue;
        }
        if in_section && line.starts_with("## ") {
            break;
        }
        if in_section && line.starts_with('|') {
            let cells = line
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !cells.is_empty()
                && !cells[0].starts_with("---")
                && !matches!(cells[0].as_str(), "ID" | "Capability" | "Item")
            {
                rows.push(cells);
            }
        }
    }
    rows
}

fn is_decision_id(id: &str) -> bool {
    id.strip_prefix('D').is_some_and(|number| {
        !number.is_empty() && number.chars().all(|digit| digit.is_ascii_digit())
    })
}

fn has_version_declaration(contents: &str) -> bool {
    contents.lines().take(6).any(|line| {
        line.contains("Version:")
            || line.split_whitespace().any(|token| {
                let token = token.trim_matches(|character: char| {
                    !character.is_ascii_digit() && character != '.'
                });
                token.contains('.')
                    && token.split('.').all(|part| {
                        !part.is_empty() && part.chars().all(|digit| digit.is_ascii_digit())
                    })
            })
    })
}

fn check_machines() -> Result<(), String> {
    let specs = load_specs()?;
    let mut global_ids = std::collections::BTreeMap::new();
    let mut transition_count = 0;
    for (path, spec) in &specs {
        transition_count += spec.transitions.len();
        for (id, name) in
            stable_id_map(spec).map_err(|error| format!("{}: {error}", path.display()))?
        {
            if let Some(existing) = global_ids.insert(id, name.clone()) {
                return Err(format!(
                    "global stable ID collision between `{existing}` and `{name}`"
                ));
            }
        }
    }
    println!(
        "machine specifications are valid: {} machines, {transition_count} transitions, {} stable IDs",
        specs.len(),
        global_ids.len()
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn check_models() -> Result<(), String> {
    let artifact = Path::new("target/model-artifacts/shortest-failure.json");
    if let Some(parent) = artifact.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create model artifact directory: {error}"))?;
    }
    let specs = load_specs()?;
    let mut snapshot_count = 0;
    for (_, spec) in &specs {
        let report = explore(spec);
        if !report.is_success(spec) {
            let (message, path) = report.failures.first().map_or_else(
                || {
                    (
                        "coverage failure without transition path".to_owned(),
                        Vec::new(),
                    )
                },
                |failure| (failure.message.clone(), failure.path.clone()),
            );
            write_exploration_failure(
                artifact,
                &spec.name,
                &json!({"max_depth": 256, "max_snapshots": 100_000}),
                &path,
                &message,
                "NEXA_MODEL_MACHINE_FAILURE",
            )?;
            return Err(format!(
                "model `{}` failed:\n{}",
                spec.name,
                report
                    .failures
                    .iter()
                    .map(|failure| failure.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        snapshot_count += report.visited_snapshots;
    }
    let system_config = SystemConfig::parse(include_str!(
        "../../../specs/systems/task_scope.system.spec"
    ))?;
    let system_report = explore_task_scope(system_config);
    if !system_report.failures.is_empty() {
        let (message, path) = &system_report.failures[0];
        write_exploration_failure(
            artifact,
            "task-scope-system",
            &json!({"max_depth": 32, "max_worlds": 16_384}),
            &path
                .iter()
                .map(|event| format!("{event:?}"))
                .collect::<Vec<_>>(),
            message,
            "NEXA_MODEL_SYSTEM_FAILURE",
        )?;
        return Err(format!(
            "TaskScope system model failed: {:?}",
            system_report.failures
        ));
    }
    let realm_config = RealmSystemConfig::parse(include_str!(
        "../../../specs/systems/realm_runtime.system.spec"
    ))?;
    let realm_report = explore_realm_runtime(realm_config);
    if !realm_report.failures.is_empty() {
        let (message, path) = &realm_report.failures[0];
        write_exploration_failure(
            artifact,
            "realm-runtime-system",
            &json!({}),
            &path
                .iter()
                .map(|event| format!("{event:?}"))
                .collect::<Vec<_>>(),
            message,
            "NEXA_MODEL_REALM_FAILURE",
        )?;
        return Err(format!(
            "RealmRuntime system model failed: {:?}",
            realm_report.failures
        ));
    }
    let mut current_realm = RealmModel::default();
    for event in [
        RealmEvent::Spawn,
        RealmEvent::Poll,
        RealmEvent::RestartReload,
        RealmEvent::LateCompletion,
    ] {
        current_realm
            .apply(event)
            .map_err(|error| format!("current Realm model rejected {event:?}: {error:?}"))?;
    }
    if !current_realm.invariants_hold() {
        return Err("current Realm model violated resource invariants".into());
    }
    let summary = std::fs::File::create("target/model-artifacts/model-check-summary.json")
        .map_err(|error| format!("could not create model summary: {error}"))?;
    serde_json::to_writer_pretty(
        summary,
        &json!({
            "format_version": 1,
            "commit_sha": current_commit_sha(),
            "status": "success",
            "machine_snapshots": snapshot_count,
            "task_scope_worlds": system_report.visited_worlds,
            "realm_worlds": realm_report.visited_worlds,
            "current_realm_paths": 1
        }),
    )
    .map_err(|error| format!("could not write model summary: {error}"))?;
    println!(
        "bounded model exploration passed: {} machines, {snapshot_count} snapshots, {} task/scope worlds, {} realm worlds, current Realm restart path passed",
        specs.len(),
        system_report.visited_worlds,
        realm_report.visited_worlds,
    );
    Ok(())
}

fn write_exploration_failure(
    path: &Path,
    model: &str,
    model_config: &Value,
    trace: &[String],
    message: &str,
    error_code: &str,
) -> Result<(), String> {
    let failure_event = trace
        .last()
        .cloned()
        .unwrap_or_else(|| "exploration".into());
    let artifact = ModelFailureArtifact {
        format_version: MODEL_FAILURE_ARTIFACT_VERSION,
        commit_sha: current_commit_sha(),
        runtime_kind: "RealmRuntime".into(),
        shadow_state_fields: 0,
        model_config: json!({
            "model": model,
            "bounds": model_config
        }),
        path: trace.to_owned(),
        failure_event,
        model_before: Value::Null,
        model_after: json!({"failure": message}),
        runtime_before: Value::Null,
        runtime_after: Value::Null,
        ledger: json!({}),
        epochs: json!({}),
        tasks: json!([]),
        requests: json!([]),
        completions: json!([]),
        releases: json!([]),
        heap: json!({}),
        roots: json!([]),
        root_publications: json!([]),
        module_handles: json!([]),
        completion_accounting: json!({}),
        failure_point_stats: json!({}),
        trace: json!(trace),
        error_code: error_code.into(),
    };
    let file = std::fs::File::create(path)
        .map_err(|error| format!("could not create model failure artifact: {error}"))?;
    write_model_failure_artifact(file, &artifact)
        .map_err(|error| format!("could not write model failure artifact: {error}"))
}

fn load_specs() -> Result<Vec<(PathBuf, MachineSpec)>, String> {
    let directory = Path::new("specs/machines");
    let mut paths = std::fs::read_dir(directory)
        .map_err(|error| format!("could not read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|entry| {
            entry
                .extension()
                .is_some_and(|extension| extension == "spec")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            MachineSpec::from_path(&path)
                .map(|spec| (path.clone(), spec))
                .map_err(|errors| {
                    errors
                        .into_iter()
                        .map(|error| format!("{}: {error}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use nexa::prelude::{
        ArrayType, BufferType, ClassType, FunctionBuilder, Instruction, MapType, ModuleBuilder,
        SectionKind, Signature, SnapshotType, SourceMapEntry, SourceSpan, StateField,
        StateHandleType, StateSchema, StateType, StructField, StructType, ValueType,
    };
    use nexa::{FileId, StableId};

    use nexa_model::artifact::{
        MODEL_FAILURE_ARTIFACT_VERSION, ModelFailureArtifact, write_model_failure_artifact,
    };
    use serde_json::json;

    use super::{
        DiagnosticFormat, build_command, check_command, exec_with_options, fixture_check,
        model_replay, render_module_dump, verify_command,
    };

    #[test]
    #[allow(clippy::too_many_lines)]
    fn complete_cli_command_paths_execute_real_components() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("nexa-cli-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("main.nexa");
        let module = directory.join("main.nxb");
        let trace = directory.join("trace.json");
        let limits = directory.join("limits.json");
        let fixture = directory.join("fixture.json");
        fs::write(&source, "fn main() -> i32 { return 7; }").unwrap();
        fs::write(
            &limits,
            r#"{"max_frame_bytes":65536,"max_immediate_cost":1024,"max_wcet_states":100000}"#,
        )
        .unwrap();
        fs::write(
            &fixture,
            r#"{"format_version":1,"stateful_domain":7,"objects":[]}"#,
        )
        .unwrap();

        check_command(
            super::cli::CheckArgs {
                input: Some(source.clone()),
                project: None,
                contract: None,
                policy: None,
                manifest_only: false,
                limits_file: Some(limits.clone()),
            },
            DiagnosticFormat::Json,
        )
        .unwrap();
        build_command(
            super::cli::BuildArgs {
                input: Some(source.clone()),
                project: None,
                contract: None,
                output: Some(module.clone()),
                limits_file: None,
                dump_source_map: true,
            },
            DiagnosticFormat::Human,
        )
        .unwrap();
        verify_command(
            super::cli::VerifyArgs {
                input: module.clone(),
                limits_file: Some(limits.clone()),
            },
            DiagnosticFormat::Human,
        )
        .unwrap();
        exec_with_options(
            super::standalone::ExecOptions {
                module: module.clone(),
                function: 0,
                runtime_arguments: Vec::new(),
                fuel: 1_000_000,
                limits_file: None,
                trace_output: Some(trace.clone()),
            },
            DiagnosticFormat::Human,
            true,
        )
        .unwrap();
        assert!(fs::read_to_string(&trace).unwrap().contains("I32(7)"));
        fixture_check(&[fixture.display().to_string()], DiagnosticFormat::Human).unwrap();

        let artifact_path = directory.join("model.json");
        let artifact = ModelFailureArtifact {
            format_version: MODEL_FAILURE_ARTIFACT_VERSION,
            commit_sha: "test".into(),
            runtime_kind: "RealmRuntime".into(),
            shadow_state_fields: 0,
            model_config: json!({"model": "realm"}),
            path: vec!["Spawn".into()],
            failure_event: "Spawn".into(),
            model_before: json!({}),
            model_after: json!({}),
            runtime_before: json!({}),
            runtime_after: json!({}),
            ledger: json!({}),
            epochs: json!({}),
            tasks: json!([]),
            requests: json!([]),
            completions: json!([]),
            releases: json!([]),
            heap: json!({}),
            roots: json!([]),
            root_publications: json!([]),
            module_handles: json!([]),
            completion_accounting: json!({}),
            failure_point_stats: json!({}),
            trace: json!([]),
            error_code: "NEXA_MODEL_TEST".into(),
        };
        let file = fs::File::create(&artifact_path).unwrap();
        write_model_failure_artifact(file, &artifact).unwrap();
        model_replay(&artifact_path).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn diagnostic_format_is_global_and_strict() {
        let parsed = super::cli::Cli::try_parse_from([
            "nexa",
            "check",
            "main.nexa",
            "--diagnostic-format",
            "json",
        ])
        .unwrap();
        assert_eq!(parsed.diagnostic_format, DiagnosticFormat::Json);
        assert!(
            super::cli::Cli::try_parse_from(["nexa", "check", "--diagnostic-format", "xml",])
                .is_err()
        );
        assert_eq!(super::CliError::diagnostic("type mismatch").exit_code(), 1);
        assert_eq!(
            super::CliError::usage("usage: nexa check <path>").exit_code(),
            2
        );
        assert_eq!(
            super::CliError::internal("could not read project").exit_code(),
            3
        );
        assert_eq!(
            super::CliError::internal("worker terminated").exit_code(),
            3
        );
    }

    #[test]
    fn compile_phase_errors_render_through_unified_diagnostic_pipeline() {
        let directory = std::env::temp_dir().join(format!(
            "nexa-cli-compile-diag-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.nexa");
        fs::write(&path, "fn main() { 1 }").unwrap();
        let source = fs::read_to_string(&path).unwrap();

        // The single-file `nexa check` path must surface typed-lowering failures
        // as a source-backed diagnostic batch instead of leaking the internal
        // Debug representation of the compile error.
        let build = super::project::virtual_snippet(&source, &path).unwrap();
        let result =
            build.compile_with_limits(1, None, &[], false, nexa::VerifierLimits::default());
        let batch = match result {
            Err(super::project::BuildCompileError::Facade(
                nexa::PackageBuildError::CompileFailed(batch),
            )) => batch,
            other => panic!("expected CompileFailed batch, got {other:?}"),
        };
        assert_eq!(batch.diagnostics().len(), 1);
        let diagnostic = &batch.diagnostics()[0];
        assert_eq!(diagnostic.code, nexa::ErrorCode::NX2101);
        assert_eq!(diagnostic.severity, nexa::Severity::Error);
        assert!(diagnostic.message.contains("type mismatch"));
        let label = diagnostic
            .primary_label()
            .expect("compile-phase diagnostic retains its primary source label");
        assert_eq!(label.range.start, 12);
        assert_eq!(label.range.end, 13);

        // The unified human renderer emits the code, the user message, and the
        // exact source line with its label.
        let rendered = nexa::LeafDiagnosticRenderer::human(&batch);
        assert!(
            rendered.contains("error[NX2101]: type mismatch"),
            "{rendered}"
        );
        assert!(rendered.contains("fn main() { 1 }"), "{rendered}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn check_command_reports_compile_errors_as_rendered_diagnostics() {
        let directory = std::env::temp_dir().join(format!(
            "nexa-cli-check-diag-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.nexa");
        fs::write(&path, "fn main() { 1 }").unwrap();

        // `nexa check` must return a diagnostic-or-test failure (exit 1) whose
        // output was already rendered by the unified pipeline.
        let error = check_command(
            super::cli::CheckArgs {
                input: Some(path),
                project: None,
                contract: None,
                policy: None,
                manifest_only: false,
                limits_file: None,
            },
            DiagnosticFormat::Human,
        )
        .unwrap_err();
        assert!(error.already_rendered(), "{error}");
        assert_eq!(error.exit_code(), 1);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn bytecode_dump_is_deterministic_and_supports_code_types_and_source_map_views() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::Return { source: 0 });
        let mut builder = ModuleBuilder::new();
        builder.string("Nexa界\n");
        let position_type = StableId::from_name("Position");
        builder.struct_type(StructType {
            type_id: position_type,
            fields: vec![StructField {
                stable_id: StableId::from_parts(&["Position", "::x"]),
                ty: ValueType::I32,
            }],
        });
        builder.class_type(ClassType {
            type_id: StableId::from_name("Node"),
            fields: vec![StructField {
                stable_id: StableId::from_parts(&["Node", "::value"]),
                ty: ValueType::I32,
            }],
        });
        let state_target = ValueType::Named(StableId::from_name("Store"));
        builder.state_handle_type(StateHandleType::new(state_target));
        builder.array_type(ArrayType::new(ValueType::I32));
        builder.map_type(MapType::new(ValueType::String, ValueType::I32));
        builder.buffer_type(BufferType::new(ValueType::I64));
        builder.snapshot_type(SnapshotType::new(position_type));
        builder.state_schema(StateSchema {
            types: vec![StateType {
                stable_id: StableId::from_name("Store"),
                version: 1,
                fields: vec![StateField {
                    stable_id: StableId::from_parts(&["Store", "::value"]),
                    ty: ValueType::I32,
                }],
            }],
        });
        builder.function(function.finish().unwrap());
        builder.source_map([
            SourceMapEntry {
                function: 0,
                pc_start: 1,
                pc_end: 2,
                span: SourceSpan::new(FileId(1), 20, 26),
            },
            SourceMapEntry {
                function: 0,
                pc_start: 0,
                pc_end: 1,
                span: SourceSpan::new(FileId(1), 10, 19),
            },
        ]);
        let module = builder.finish();
        let bytes = module.encode();

        let full = render_module_dump(&bytes, &module, None, false).unwrap();
        assert_eq!(
            full,
            render_module_dump(&bytes, &module, None, false).unwrap()
        );
        assert!(full.contains("header magic=NXBC version=7 sections=16"));
        assert!(full.contains("000000 LoadI32"));
        assert!(full.contains("string 0 \"Nexa界\\n\""));
        assert!(full.contains("struct "));
        assert!(full.contains("field index=0"));
        assert!(full.contains("class "));
        assert!(full.contains("mutable=true"));
        assert!(full.contains("state-handle "));
        assert!(full.contains("array "));
        assert!(full.contains("element=I32"));
        assert!(full.contains("map "));
        assert!(full.contains("key=String value=I32"));
        assert!(full.contains("buffer "));
        assert!(full.contains("element=I64 ownership=vm-copy"));
        assert!(full.contains("snapshot "));
        assert!(full.contains("ownership=host immutable=true"));
        assert!(full.contains("state-class "));
        assert!(full.contains("persistent=true"));
        assert!(
            full.find("pc=0..1")
                .expect("first source map entry is present")
                < full
                    .find("pc=1..2")
                    .expect("second source map entry is present")
        );

        let types = render_module_dump(&bytes, &module, Some(SectionKind::Types), false).unwrap();
        assert!(types.contains("section types"));
        assert!(types.contains("state-handle "));
        assert!(types.contains("array "));
        assert!(types.contains("map "));
        assert!(types.contains("buffer "));
        assert!(types.contains("snapshot "));
        assert!(!types.contains("code function="));
        let code = render_module_dump(&bytes, &module, Some(SectionKind::Code), false).unwrap();
        assert!(code.contains("section code"));
        assert!(code.contains("000001 Return"));
        let source_map = render_module_dump(&bytes, &module, None, true).unwrap();
        assert!(source_map.starts_with("source-map function=0 pc=0..1"));
    }
}
