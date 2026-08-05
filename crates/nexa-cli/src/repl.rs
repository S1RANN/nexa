use std::collections::VecDeque;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{CliError, CliResult, DiagnosticFormat, project};

pub(crate) const REPL_PACKAGE_ID: &str = "nexa.repl";

#[derive(Default)]
pub(crate) struct StdioReplConsole {
    prepared: Vec<nexa::ReplConsoleEmission>,
}

impl nexa::ReplConsoleHost for StdioReplConsole {
    fn prepare_cell(
        &mut self,
        output: &[nexa::ReplConsoleEmission],
    ) -> Result<(), nexa::ReplConsoleHostError> {
        self.prepared.clear();
        self.prepared.extend_from_slice(output);
        Ok(())
    }

    fn commit_prepared_cell(&mut self) {
        fn write(
            writer: &mut dyn std::io::Write,
            text: &str,
            line_terminated: bool,
        ) -> std::io::Result<()> {
            writer.write_all(text.as_bytes())?;
            if line_terminated {
                writer.write_all(b"\n")?;
            }
            writer.flush()
        }

        for emission in self.prepared.drain(..) {
            let _ = match emission.stream() {
                nexa::ReplConsoleStream::Stdout => write(
                    &mut std::io::stdout().lock(),
                    emission.text(),
                    emission.line_terminated(),
                ),
                nexa::ReplConsoleStream::Stderr => write(
                    &mut std::io::stderr().lock(),
                    emission.text(),
                    emission.line_terminated(),
                ),
            };
        }
    }

    fn discard_prepared_cell(&mut self) {
        self.prepared.clear();
    }
}

/// Production adapter from the line-oriented CLI driver to the owning façade session.
///
/// The adapter constructs a fresh immutable `ResolvedBuildInput` for each reader-facing Cell and
/// then hands the internal `SourceKey` plus the original `SourceIdentity`/text to the façade. The
/// façade remains the sole owner of cumulative analysis, compilation, verification, Realm reload,
/// hidden state, and commit/rollback.
pub(crate) struct CanonicalReplBackend {
    session: nexa::ReplSession,
    diagnostic_format: DiagnosticFormat,
}

impl CanonicalReplBackend {
    pub(crate) fn new(
        limits: ReplLimits,
        diagnostic_format: DiagnosticFormat,
    ) -> Result<Self, String> {
        let limits = nexa::ReplSessionLimits {
            max_heap_objects: limits.heap_objects,
            fuel_per_cell: limits.cell_fuel,
            max_committed_cells: limits.committed_cells,
            max_diagnostic_history: limits.diagnostic_history,
            max_output_bytes_per_cell: limits.output_bytes,
        };
        let session = nexa::ReplSession::new(limits, Box::new(StdioReplConsole::default()))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            session,
            diagnostic_format,
        })
    }

    fn with_resolved_cell<T>(
        &mut self,
        ordinal: u64,
        identity: &nexa::SourceIdentity,
        source: &str,
        operation: impl FnOnce(
            &mut nexa::ReplSession,
            nexa::ReplResolvedCellInput<'_, '_>,
        ) -> Result<T, nexa::ReplSessionError>,
    ) -> Result<T, String> {
        let build = project::virtual_repl_cell_with_identity(ordinal, source, identity.clone())
            .map_err(|error| error.to_string())?;
        let origin = build.virtual_source_origin.as_ref().ok_or_else(|| {
            "resolved REPL Cell omitted its internal/display source origin".to_owned()
        })?;
        if origin.display_identity != *identity
            || origin.original_text.as_ref() != source
            || !origin.source_text_is_original
        {
            return Err(
                "resolved REPL Cell did not preserve the reader-facing source authority".into(),
            );
        }
        let contract = build
            .host_contract
            .input()
            .map_err(|error| error.to_string())?;
        let candidate = build.identity(ordinal).map_err(|error| error.to_string())?;
        let cell =
            nexa_analysis::ReplCellInput::new(ordinal, identity.clone(), Arc::<str>::from(source));
        operation(
            &mut self.session,
            nexa::ReplResolvedCellInput {
                build_input: build.input.as_ref(),
                contract: &contract,
                identity: candidate,
                source_key: &origin.source_key,
                cell,
            },
        )
        .map_err(|error| render_session_error(error, self.diagnostic_format))
    }
}

impl ReplBackend for CanonicalReplBackend {
    fn is_complete(
        &mut self,
        _identity: &nexa::SourceIdentity,
        source: &str,
    ) -> Result<bool, String> {
        nexa_syntax::classify_cell_completeness(source)
            .map(|completeness| completeness == nexa_syntax::CellCompleteness::Complete)
            .map_err(|error| error.to_string())
    }

    #[allow(clippy::result_large_err)]
    fn evaluate(
        &mut self,
        cell: u64,
        identity: &nexa::SourceIdentity,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<ReplEvaluation, String> {
        self.with_resolved_cell(cell, identity, source, |session, input| {
            session.submit_cell(input, cancelled)
        })
        .map(|outcome| ReplEvaluation {
            rendered_value: outcome.rendered_value,
        })
    }

    #[allow(clippy::result_large_err)]
    fn type_of(
        &mut self,
        cell: u64,
        identity: &nexa::SourceIdentity,
        source: &str,
    ) -> Result<String, String> {
        self.with_resolved_cell(cell, identity, source, |session, input| {
            session.inspect_cell_type(input)
        })
    }

    fn ast(&mut self, identity: &nexa::SourceIdentity, source: &str) -> Result<String, String> {
        self.session
            .ast(identity, source)
            .map_err(|error| render_session_error(error, self.diagnostic_format))
    }

    fn bytecode(&mut self, name: Option<&str>) -> Result<String, String> {
        self.session
            .bytecode(name)
            .map_err(|error| render_session_error(error, self.diagnostic_format))
    }

    fn collect_garbage(&mut self) -> Result<String, String> {
        self.session
            .collect_garbage()
            .map(|report| {
                format!(
                    "collections={}, reclaimed_objects={}, live_objects={}",
                    report.collections, report.reclaimed_objects, report.live_objects
                )
            })
            .map_err(|error| render_session_error(error, self.diagnostic_format))
    }

    fn memory(&self) -> Result<String, String> {
        let report = self.session.memory();
        Ok(format!(
            "max_heap_objects={}, live_heap_objects={}, committed_cells={}, \
             retained_diagnostic_batches={}",
            report.max_heap_objects,
            report.live_heap_objects,
            report.committed_cells,
            report.retained_diagnostic_batches
        ))
    }

    fn reset(&mut self) -> Result<(), String> {
        self.session
            .reset()
            .map_err(|error| render_session_error(error, self.diagnostic_format))
    }
}

fn render_session_error(error: nexa::ReplSessionError, format: DiagnosticFormat) -> String {
    match error {
        nexa::ReplSessionError::Analysis { diagnostics, .. }
        | nexa::ReplSessionError::Build(nexa::PackageBuildError::AnalysisFailed(diagnostics)) => {
            match format {
                DiagnosticFormat::Human => crate::render_human_batch(&diagnostics),
                DiagnosticFormat::Json => nexa::LeafDiagnosticRenderer::json(&diagnostics)
                    .unwrap_or_else(|render_error| {
                        format!("could not render REPL diagnostics: {render_error}")
                    }),
                DiagnosticFormat::Ndjson => nexa::LeafDiagnosticRenderer::ndjson(&diagnostics)
                    .unwrap_or_else(|render_error| {
                        format!("could not render REPL diagnostics: {render_error}")
                    }),
            }
        }
        error => error.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplLimits {
    pub heap_objects: u32,
    pub cell_fuel: u64,
    pub committed_cells: usize,
    pub diagnostic_history: usize,
    pub output_bytes: usize,
}

impl Default for ReplLimits {
    fn default() -> Self {
        Self {
            heap_objects: 4_096,
            cell_fuel: 20_000,
            committed_cells: 1_024,
            diagnostic_history: 256,
            output_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplOptions {
    pub limits: ReplLimits,
    pub prompt: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplCommand {
    Type(String),
    Ast(String),
    Bytecode(Option<String>),
    Gc,
    Memory,
    Load(PathBuf),
    Reset,
    Help,
    Quit,
}

pub(crate) fn parse_command(source: &str) -> Result<Option<ReplCommand>, String> {
    let source = source.trim();
    if !source.starts_with(':') {
        return Ok(None);
    }
    let (name, rest) = source
        .split_once(char::is_whitespace)
        .map_or((source, ""), |(name, rest)| (name, rest.trim()));
    let required_source = |command: &str| {
        (!rest.is_empty())
            .then(|| rest.to_owned())
            .ok_or_else(|| format!("`{command}` requires source"))
    };
    let no_arguments = |command: &str| {
        if rest.is_empty() {
            Ok(())
        } else {
            Err(format!("`{command}` does not accept arguments"))
        }
    };
    match name {
        ":type" => required_source(name).map(ReplCommand::Type).map(Some),
        ":ast" => required_source(name).map(ReplCommand::Ast).map(Some),
        ":bytecode" => Ok(Some(ReplCommand::Bytecode(
            (!rest.is_empty()).then(|| rest.to_owned()),
        ))),
        ":gc" => no_arguments(name).map(|()| Some(ReplCommand::Gc)),
        ":memory" => no_arguments(name).map(|()| Some(ReplCommand::Memory)),
        ":load" => required_source(name)
            .map(PathBuf::from)
            .map(ReplCommand::Load)
            .map(Some),
        ":reset" => no_arguments(name).map(|()| Some(ReplCommand::Reset)),
        ":help" => no_arguments(name).map(|()| Some(ReplCommand::Help)),
        ":quit" => no_arguments(name).map(|()| Some(ReplCommand::Quit)),
        unknown => Err(format!("unknown REPL command `{unknown}`; use `:help`")),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReplEvaluation {
    pub rendered_value: Option<String>,
}

/// Backend boundary for the canonical frontend/runtime pipeline.
///
/// `evaluate` is transactional: on any error it must leave the previously committed package and
/// hidden session state untouched. Implementations compile a cell through syntax, analysis,
/// typed IR, bytecode and verifier before running it. The backend owns the frozen Runtime,
/// fuel, heap, and combined Console-output budgets and must reject an over-limit staged cell
/// before committing it. This trait is deliberately not an expression-evaluator interface.
pub(crate) trait ReplBackend {
    fn is_complete(
        &mut self,
        identity: &nexa::SourceIdentity,
        source: &str,
    ) -> Result<bool, String>;

    fn evaluate(
        &mut self,
        cell: u64,
        identity: &nexa::SourceIdentity,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<ReplEvaluation, String>;

    fn type_of(
        &mut self,
        cell: u64,
        identity: &nexa::SourceIdentity,
        source: &str,
    ) -> Result<String, String>;

    fn ast(&mut self, identity: &nexa::SourceIdentity, source: &str) -> Result<String, String>;

    fn bytecode(&mut self, name: Option<&str>) -> Result<String, String>;

    fn collect_garbage(&mut self) -> Result<String, String>;

    fn memory(&self) -> Result<String, String>;

    fn reset(&mut self) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplAction {
    Continue { output: String },
    Quit,
}

pub(crate) struct ReplDriver<B> {
    backend: B,
    limits: ReplLimits,
    next_cell: u64,
    committed_cells: usize,
    diagnostics: VecDeque<String>,
}

impl<B: ReplBackend> ReplDriver<B> {
    pub(crate) fn new(backend: B, limits: ReplLimits) -> Self {
        Self {
            backend,
            limits,
            next_cell: 1,
            committed_cells: 0,
            diagnostics: VecDeque::with_capacity(limits.diagnostic_history),
        }
    }

    pub(crate) fn submit(
        &mut self,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<ReplAction, String> {
        let identity = self.next_cell_identity();
        self.submit_with_identity(&identity, source, cancelled)
    }

    fn is_complete(&mut self, source: &str) -> Result<bool, String> {
        let identity = self.next_cell_identity();
        self.backend.is_complete(&identity, source)
    }

    fn next_cell_identity(&self) -> nexa::SourceIdentity {
        nexa::SourceIdentity::package(REPL_PACKAGE_ID, format!("repl::cell_{}", self.next_cell))
    }

    fn submit_with_identity(
        &mut self,
        identity: &nexa::SourceIdentity,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<ReplAction, String> {
        if let Some(command) = parse_command(source)? {
            return self.command(command, cancelled);
        }
        self.submit_cell_with_identity(identity, source, cancelled)
    }

    fn submit_cell_with_identity(
        &mut self,
        identity: &nexa::SourceIdentity,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<ReplAction, String> {
        if source.trim().is_empty() {
            return Ok(ReplAction::Continue {
                output: String::new(),
            });
        }
        if self.committed_cells == self.limits.committed_cells {
            return self.fail(format!(
                "REPL committed Cell limit {} reached; use `:reset`",
                self.limits.committed_cells
            ));
        }
        let cell = self.next_cell;
        self.next_cell = self.next_cell.saturating_add(1);
        let evaluation = match self.backend.evaluate(cell, identity, source, cancelled) {
            Ok(evaluation) => evaluation,
            Err(error) => return self.fail(error),
        };
        self.committed_cells = self.committed_cells.saturating_add(1);
        Ok(ReplAction::Continue {
            output: evaluation.rendered_value.unwrap_or_default(),
        })
    }

    fn command(
        &mut self,
        command: ReplCommand,
        cancelled: &AtomicBool,
    ) -> Result<ReplAction, String> {
        let output = match command {
            ReplCommand::Type(source) => self.backend.type_of(
                self.next_cell,
                &nexa::SourceIdentity::package(REPL_PACKAGE_ID, "repl::inspection"),
                &source,
            ),
            ReplCommand::Ast(source) => self.backend.ast(
                &nexa::SourceIdentity::package(REPL_PACKAGE_ID, "repl::inspection"),
                &source,
            ),
            ReplCommand::Bytecode(name) => self.backend.bytecode(name.as_deref()),
            ReplCommand::Gc => self
                .backend
                .collect_garbage()
                .map(|report| format!("gc: {report}")),
            ReplCommand::Memory => self
                .backend
                .memory()
                .map(|report| format!("memory: {report}")),
            ReplCommand::Load(path) => {
                let source = std::fs::read_to_string(&path)
                    .map_err(|error| format!("could not read {}: {error}", path.display()))?;
                let identity = nexa::SourceIdentity::package(
                    REPL_PACKAGE_ID,
                    path.canonicalize()
                        .unwrap_or(path)
                        .to_string_lossy()
                        .into_owned(),
                );
                return self.submit_cell_with_identity(&identity, &source, cancelled);
            }
            ReplCommand::Reset => {
                self.backend.reset()?;
                self.next_cell = 1;
                self.committed_cells = 0;
                self.diagnostics.clear();
                Ok("session reset".to_owned())
            }
            ReplCommand::Help => Ok(help_text().to_owned()),
            ReplCommand::Quit => return Ok(ReplAction::Quit),
        };
        match output {
            Ok(output) => Ok(ReplAction::Continue { output }),
            Err(error) => self.fail(error),
        }
    }

    fn fail<T>(&mut self, error: String) -> Result<T, String> {
        if self.diagnostics.len() == self.limits.diagnostic_history {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(error.clone());
        Err(error)
    }

    #[cfg(test)]
    fn committed_cells(&self) -> usize {
        self.committed_cells
    }

    #[cfg(test)]
    fn diagnostics(&self) -> &VecDeque<String> {
        &self.diagnostics
    }
}

pub(crate) fn install_cancel_handler() -> CliResult<Arc<AtomicBool>> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Release);
    })
    .map_err(|error| {
        CliError::internal(format!("could not install REPL Ctrl+C handler: {error}"))
    })?;
    Ok(cancelled)
}

pub(crate) fn run<B: ReplBackend>(
    backend: B,
    options: ReplOptions,
    cancelled: &AtomicBool,
) -> CliResult<()> {
    let stdin = std::io::stdin();
    let interactive = options.prompt && stdin.is_terminal() && std::io::stdout().is_terminal();
    let mut input = stdin.lock();
    let mut session = ReplDriver::new(backend, options.limits);
    let mut pending = String::new();
    loop {
        if interactive {
            if pending.is_empty() {
                print!("nexa> ");
            } else {
                print!("....> ");
            }
            std::io::stdout().flush().map_err(|error| {
                CliError::internal(format!("could not flush REPL prompt: {error}"))
            })?;
        }
        let mut line = String::new();
        let read = match input.read_line(&mut line) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                cancelled.store(false, Ordering::Release);
                pending.clear();
                eprintln!("error: cancelled");
                continue;
            }
            Err(error) => {
                return Err(CliError::internal(format!(
                    "could not read REPL input: {error}"
                )));
            }
        };
        if read == 0 {
            if !pending.trim().is_empty() {
                match session.submit(&pending, cancelled) {
                    Ok(ReplAction::Continue { output }) => {
                        if !output.is_empty() {
                            println!("{output}");
                        }
                    }
                    Ok(ReplAction::Quit) => {}
                    Err(error) => print_repl_error(&error),
                }
            }
            return Ok(());
        }
        if interactive && cancelled.swap(false, Ordering::AcqRel) && pending.is_empty() {
            eprintln!("error: cancelled");
            if line.trim().is_empty() {
                continue;
            }
        }
        pending.push_str(&line);
        let complete = if pending.trim_start().starts_with(':') {
            true
        } else {
            session.is_complete(&pending).map_err(CliError::internal)?
        };
        if !complete {
            continue;
        }
        match session.submit(&pending, cancelled) {
            Ok(ReplAction::Continue { output }) => {
                if !output.is_empty() {
                    println!("{output}");
                }
            }
            Ok(ReplAction::Quit) => return Ok(()),
            Err(error) => print_repl_error(&error),
        }
        cancelled.store(false, Ordering::Release);
        pending.clear();
    }
}

/// Prints one REPL error. Already-rendered diagnostic batches start with their own
/// `error[NX...]`/`warning[NX...]` header, so they must not gain an extra `error: ` prefix.
fn print_repl_error(error: &str) {
    if error.starts_with("error[") || error.starts_with("warning[") {
        eprintln!("{error}");
    } else {
        eprintln!("error: {error}");
    }
}

pub(crate) const fn help_text() -> &'static str {
    ":type <expr>\n\
     :ast <source>\n\
     :bytecode [name]\n\
     :gc\n\
     :memory\n\
     :load <file>\n\
     :reset\n\
     :help\n\
     :quit"
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{
        ReplAction, ReplBackend, ReplCommand, ReplDriver, ReplEvaluation, ReplLimits, parse_command,
    };

    #[derive(Default)]
    struct Backend {
        values: Vec<String>,
        fail: bool,
        reset_count: usize,
    }

    impl ReplBackend for Backend {
        fn is_complete(
            &mut self,
            _identity: &nexa::SourceIdentity,
            _source: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }

        fn evaluate(
            &mut self,
            _cell: u64,
            _identity: &nexa::SourceIdentity,
            source: &str,
            cancelled: &AtomicBool,
        ) -> Result<ReplEvaluation, String> {
            if cancelled.swap(false, Ordering::AcqRel) {
                return Err("cancelled".into());
            }
            if self.fail {
                self.fail = false;
                return Err("compile failed".into());
            }
            self.values.push(source.to_owned());
            Ok(ReplEvaluation {
                rendered_value: Some(source.to_owned()),
            })
        }

        fn type_of(
            &mut self,
            _cell: u64,
            _identity: &nexa::SourceIdentity,
            source: &str,
        ) -> Result<String, String> {
            Ok(format!("type({source})"))
        }

        fn ast(
            &mut self,
            _identity: &nexa::SourceIdentity,
            source: &str,
        ) -> Result<String, String> {
            Ok(format!("ast({source})"))
        }

        fn bytecode(&mut self, name: Option<&str>) -> Result<String, String> {
            Ok(format!("bytecode({})", name.unwrap_or("*")))
        }

        fn collect_garbage(&mut self) -> Result<String, String> {
            Ok("complete".into())
        }

        fn memory(&self) -> Result<String, String> {
            Ok("0 bytes".into())
        }

        fn reset(&mut self) -> Result<(), String> {
            self.values.clear();
            self.reset_count += 1;
            Ok(())
        }
    }

    #[test]
    fn default_limits_are_frozen() {
        let limits = ReplLimits::default();
        assert_eq!(limits.heap_objects, 4_096);
        assert_eq!(limits.cell_fuel, 20_000);
        assert_eq!(limits.committed_cells, 1_024);
        assert_eq!(limits.diagnostic_history, 256);
        assert_eq!(limits.output_bytes, 1024 * 1024);
    }

    #[test]
    fn every_required_command_has_one_unambiguous_shape() {
        assert_eq!(
            parse_command(":type value").unwrap(),
            Some(ReplCommand::Type("value".into()))
        );
        assert_eq!(
            parse_command(":bytecode").unwrap(),
            Some(ReplCommand::Bytecode(None))
        );
        assert_eq!(
            parse_command(":load session.nexa").unwrap(),
            Some(ReplCommand::Load("session.nexa".into()))
        );
        assert!(parse_command(":reset now").is_err());
        assert!(parse_command(":unknown").is_err());
    }

    #[test]
    fn failed_and_cancelled_cells_do_not_commit_and_later_cells_continue() {
        let cancelled = AtomicBool::new(false);
        let mut session = ReplDriver::new(Backend::default(), ReplLimits::default());
        session.submit("first", &cancelled).unwrap();
        session.backend.fail = true;
        assert_eq!(
            session.submit("failed", &cancelled).unwrap_err(),
            "compile failed"
        );
        assert_eq!(session.committed_cells(), 1);
        cancelled.store(true, Ordering::Release);
        assert_eq!(
            session.submit("cancelled", &cancelled).unwrap_err(),
            "cancelled"
        );
        assert_eq!(session.committed_cells(), 1);
        assert_eq!(
            session.submit("second", &cancelled).unwrap(),
            ReplAction::Continue {
                output: "second".into()
            }
        );
        assert_eq!(session.committed_cells(), 2);
        assert_eq!(session.backend.values, ["first", "second"]);
    }

    #[test]
    fn reset_clears_state_cells_and_diagnostics() {
        let cancelled = AtomicBool::new(false);
        let mut session = ReplDriver::new(
            Backend {
                fail: true,
                ..Backend::default()
            },
            ReplLimits::default(),
        );
        assert!(session.submit("bad", &cancelled).is_err());
        assert_eq!(session.diagnostics().len(), 1);
        session.submit(":reset", &cancelled).unwrap();
        assert_eq!(session.committed_cells(), 0);
        assert!(session.diagnostics().is_empty());
        assert_eq!(session.backend.reset_count, 1);
    }

    #[test]
    fn output_limit_is_owned_by_the_transactional_backend() {
        let cancelled = AtomicBool::new(false);
        let limits = ReplLimits {
            output_bytes: 5,
            ..ReplLimits::default()
        };
        let mut session = ReplDriver::new(Backend::default(), limits);
        let ReplAction::Continue { output } = session.submit("ééé", &cancelled).unwrap() else {
            panic!("cell continues");
        };
        assert_eq!(output, "ééé");
    }
}
