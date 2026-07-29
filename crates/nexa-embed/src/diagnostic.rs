use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Write as _};

use nexa::{Diagnostic, DiagnosticCode, ErrorCode, Severity};
use nexa_core::SourceSpan;
use nexa_runtime::RuntimeMessage;
use serde::Serialize;

use crate::manifest::{PackageId, SourceId};
use crate::source_file::{SourceFile, SourceFileRegistry, SourceRange};

const DEFAULT_PER_PACKAGE_DIAGNOSTICS: usize = 64;
const DEFAULT_ENGINE_DIAGNOSTICS: usize = 512;
const MAX_FORMATTED_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineDiagnosticStage {
    SourceDiscovery,
    Manifest,
    Policy,
    Entitlement,
    Parse,
    TypeCheck,
    Compile,
    Verify,
    Load,
    Export,
    Handler,
    Migration,
    Activation,
    Reload,
    Runtime,
    Resource,
    Persistence,
    Shutdown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineDiagnosticContext {
    pub export: Option<String>,
    pub event: Option<String>,
    pub module_epoch: Option<u64>,
    pub task: Option<EngineTaskId>,
    pub candidate_generation: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EngineTaskId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedDiagnostic {
    pub message: String,
    pub file: Option<SourceFile>,
    pub span: Option<SourceSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineDiagnostic {
    pub sequence: u64,
    pub package_id: Option<PackageId>,
    pub source_id: Option<SourceId>,
    pub stage: EngineDiagnosticStage,
    pub diagnostic: Diagnostic,
    pub file: Option<SourceFile>,
    pub related: Vec<RelatedDiagnostic>,
    pub context: EngineDiagnosticContext,
    pub fixes: Vec<String>,
}

impl EngineDiagnostic {
    #[must_use]
    pub fn without_source(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        code: DiagnosticCode,
        message: impl AsRef<str>,
    ) -> Self {
        Self {
            sequence: 0,
            package_id,
            source_id,
            stage,
            diagnostic: Diagnostic::without_source(
                code,
                Severity::Error,
                RuntimeMessage::inline(message.as_ref()),
            ),
            file: None,
            related: Vec::new(),
            context: EngineDiagnosticContext::default(),
            fixes: Vec::new(),
        }
    }

    #[must_use]
    pub fn from_leaf(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        diagnostic: Diagnostic,
        sources: Option<&SourceFileRegistry>,
    ) -> Self {
        let file = diagnostic
            .primary
            .as_ref()
            .and_then(|label| sources?.file(label.span.file))
            .cloned();
        let related = diagnostic
            .secondary
            .iter()
            .map(|label| RelatedDiagnostic {
                message: label.message.to_string(),
                file: sources
                    .and_then(|registry| registry.file(label.span.file))
                    .cloned(),
                span: Some(label.span),
            })
            .collect();
        Self {
            sequence: 0,
            package_id,
            source_id,
            stage,
            diagnostic,
            file,
            related,
            context: EngineDiagnosticContext::default(),
            fixes: Vec::new(),
        }
    }

    #[must_use]
    pub fn summary(&self) -> EngineDiagnosticSummary {
        EngineDiagnosticSummary {
            sequence: self.sequence,
            package_id: self.package_id.clone(),
            stage: self.stage,
            code: self.diagnostic.code,
            severity: self.diagnostic.severity,
            message: bounded_message(&self.diagnostic.message.to_string()),
            file: self.file.as_ref().map(|file| file.path.clone()),
            range: self
                .diagnostic
                .primary
                .as_ref()
                .and_then(|label| source_range(self.file.as_ref(), label.span)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineDiagnosticSummary {
    pub sequence: u64,
    pub package_id: Option<PackageId>,
    pub stage: EngineDiagnosticStage,
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub message: String,
    pub file: Option<String>,
    pub range: Option<SourceRange>,
}

pub struct DiagnosticRenderer;

impl DiagnosticRenderer {
    #[must_use]
    pub fn human(diagnostic: &EngineDiagnostic) -> String {
        let mut output = format!(
            "{}[{}] {:?}: {}",
            diagnostic.diagnostic.severity.as_str(),
            diagnostic.diagnostic.code,
            diagnostic.stage,
            bounded_message(&diagnostic.diagnostic.message.to_string())
        );
        if let Some(package_id) = &diagnostic.package_id {
            write!(output, "\nPackage: {package_id}").expect("String writes do not fail");
        }
        if let (Some(file), Some(label)) = (&diagnostic.file, &diagnostic.diagnostic.primary) {
            let (line, column) = file.line_column(label.span.start as usize);
            write!(output, "\n{}:{line}:{column}", file.path).expect("String writes do not fail");
            if let Some(source_line) = file.line_text(line) {
                write!(output, "\n  {source_line}").expect("String writes do not fail");
            }
            write!(output, "\n  {}", label.message).expect("String writes do not fail");
        }
        for related in &diagnostic.related {
            write!(output, "\nrelated: {}", related.message).expect("String writes do not fail");
            if let (Some(file), Some(span)) = (&related.file, related.span) {
                let (line, column) = file.line_column(span.start as usize);
                write!(output, " at {}:{line}:{column}", file.path)
                    .expect("String writes do not fail");
            }
        }
        for note in &diagnostic.diagnostic.notes {
            write!(output, "\nnote: {note}").expect("String writes do not fail");
        }
        for fix in &diagnostic.fixes {
            write!(output, "\nhelp: {}", bounded_message(fix)).expect("String writes do not fail");
        }
        output
    }

    pub fn json(diagnostic: &EngineDiagnostic) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&DiagnosticOutput::from(diagnostic))
    }

    pub fn ndjson<'a>(
        diagnostics: impl IntoIterator<Item = &'a EngineDiagnostic>,
    ) -> Result<String, serde_json::Error> {
        let mut output = String::new();
        for diagnostic in diagnostics {
            output.push_str(&serde_json::to_string(&DiagnosticOutput::from(diagnostic))?);
            output.push('\n');
        }
        Ok(output)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticOutput {
    schema: u32,
    sequence: u64,
    code: &'static str,
    severity: &'static str,
    stage: EngineDiagnosticStage,
    package_id: Option<String>,
    source_id: Option<String>,
    file: Option<String>,
    range: Option<RangeOutput>,
    message: String,
    related: Vec<RelatedOutput>,
    notes: Vec<String>,
    fixes: Vec<String>,
    context: ContextOutput,
}

#[derive(Clone, Copy, Serialize)]
struct PositionOutput {
    line: u32,
    character: u32,
}

#[derive(Clone, Copy, Serialize)]
struct RangeOutput {
    start: PositionOutput,
    end: PositionOutput,
}

#[derive(Serialize)]
struct RelatedOutput {
    message: String,
    file: Option<String>,
    range: Option<RangeOutput>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContextOutput {
    export: Option<String>,
    event: Option<String>,
    module_epoch: Option<u64>,
    task: Option<u64>,
    candidate_generation: Option<u64>,
}

impl From<&EngineDiagnostic> for DiagnosticOutput {
    fn from(engine: &EngineDiagnostic) -> Self {
        let range = engine
            .diagnostic
            .primary
            .as_ref()
            .and_then(|label| source_range(engine.file.as_ref(), label.span))
            .map(RangeOutput::from);
        Self {
            schema: 1,
            sequence: engine.sequence,
            code: engine.diagnostic.code.as_str(),
            severity: engine.diagnostic.severity.as_str(),
            stage: engine.stage,
            package_id: engine.package_id.as_ref().map(ToString::to_string),
            source_id: engine.source_id.as_ref().map(ToString::to_string),
            file: engine.file.as_ref().map(|file| file.path.clone()),
            range,
            message: bounded_message(&engine.diagnostic.message.to_string()),
            related: engine
                .related
                .iter()
                .map(|related| RelatedOutput {
                    message: bounded_message(&related.message),
                    file: related.file.as_ref().map(|file| file.path.clone()),
                    range: related
                        .span
                        .and_then(|span| source_range(related.file.as_ref(), span))
                        .map(RangeOutput::from),
                })
                .collect(),
            notes: engine
                .diagnostic
                .notes
                .iter()
                .map(ToString::to_string)
                .map(|note| bounded_message(&note))
                .collect(),
            fixes: engine
                .fixes
                .iter()
                .map(|fix| bounded_message(fix))
                .collect(),
            context: ContextOutput {
                export: engine.context.export.clone(),
                event: engine.context.event.clone(),
                module_epoch: engine.context.module_epoch,
                task: engine.context.task.map(|task| task.0),
                candidate_generation: engine.context.candidate_generation,
            },
        }
    }
}

impl From<SourceRange> for RangeOutput {
    fn from(range: SourceRange) -> Self {
        Self {
            start: PositionOutput {
                line: range.start.line,
                character: range.start.character,
            },
            end: PositionOutput {
                line: range.end.line,
                character: range.end.character,
            },
        }
    }
}

fn source_range(file: Option<&SourceFile>, span: SourceSpan) -> Option<SourceRange> {
    let file = file.filter(|file| file.id == span.file)?;
    Some(SourceRange {
        start: file.lsp_position(span.start as usize),
        end: file.lsp_position(span.end as usize),
    })
}

fn bounded_message(message: &str) -> String {
    if message.len() <= MAX_FORMATTED_MESSAGE_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_FORMATTED_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

pub(crate) struct BoundedDiagnosticLog {
    entries: VecDeque<EngineDiagnostic>,
    per_package: BTreeMap<Option<PackageId>, usize>,
    per_package_limit: usize,
    engine_limit: usize,
    next_sequence: u64,
    dropped: u64,
}

impl Default for BoundedDiagnosticLog {
    fn default() -> Self {
        Self::new(DEFAULT_PER_PACKAGE_DIAGNOSTICS, DEFAULT_ENGINE_DIAGNOSTICS)
    }
}

impl BoundedDiagnosticLog {
    #[must_use]
    pub fn new(per_package_limit: usize, engine_limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            per_package: BTreeMap::new(),
            per_package_limit: per_package_limit.max(1),
            engine_limit: engine_limit.max(1),
            next_sequence: 1,
            dropped: 0,
        }
    }

    pub fn push(&mut self, mut diagnostic: EngineDiagnostic) -> EngineDiagnostic {
        diagnostic.sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let key = diagnostic.package_id.clone();
        while self.per_package.get(&key).copied().unwrap_or(0) >= self.per_package_limit {
            if !self.remove_oldest_for(&key) {
                break;
            }
        }
        while self.entries.len() >= self.engine_limit {
            self.pop_front();
        }
        self.entries.push_back(diagnostic.clone());
        *self.per_package.entry(key).or_default() += 1;
        diagnostic
    }

    #[must_use]
    pub fn entries(&self) -> Vec<EngineDiagnostic> {
        self.entries.iter().cloned().collect()
    }

    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub fn dropped_summary(&self) -> Option<EngineDiagnosticSummary> {
        (self.dropped > 0).then(|| EngineDiagnosticSummary {
            sequence: 0,
            package_id: None,
            stage: EngineDiagnosticStage::Runtime,
            code: ErrorCode::NX7303,
            severity: Severity::Warning,
            message: format!(
                "{} older diagnostics were discarded by the bounded log",
                self.dropped
            ),
            file: None,
            range: None,
        })
    }

    #[allow(clippy::ref_option)]
    fn remove_oldest_for(&mut self, key: &Option<PackageId>) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|diagnostic| &diagnostic.package_id == key)
        else {
            return false;
        };
        let removed = self.entries.remove(index).expect("located entry exists");
        self.decrement(&removed.package_id);
        self.dropped = self.dropped.saturating_add(1);
        true
    }

    fn pop_front(&mut self) {
        if let Some(removed) = self.entries.pop_front() {
            self.decrement(&removed.package_id);
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    #[allow(clippy::ref_option)]
    fn decrement(&mut self, key: &Option<PackageId>) {
        if let Some(count) = self.per_package.get_mut(key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_package.remove(key);
            }
        }
    }
}

impl fmt::Debug for BoundedDiagnosticLog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedDiagnosticLog")
            .field("entries", &self.entries.len())
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}
