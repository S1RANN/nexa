use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use nexa::prelude::SourceSpan;
use nexa::{
    ByteRange, Diagnostic, DiagnosticBatch, DiagnosticCode, DiagnosticPhase, ErrorCode, FileId,
    LabelStyle, LeafDiagnostic, LeafRelatedLocation, MACHINE_POSITION_ENCODING,
    PackageSourceSnapshot, RuntimeMessage, Severity, SourceIdentity, SourceSnapshot,
    SourceSnapshotRegistry, TextEditSuggestion,
};
use serde::Serialize;

use crate::manifest::{PackageId, SourceId};
use crate::source_file::{SourceFileRegistry, SourcePosition, SourceRange};

const DEFAULT_PER_PACKAGE_DIAGNOSTICS: usize = 64;
const DEFAULT_ENGINE_DIAGNOSTICS: usize = 512;
const MAX_FORMATTED_MESSAGE_BYTES: usize = 64 * 1024;

static SOURCE_SNAPSHOT_CACHE: OnceLock<Mutex<Vec<Weak<EngineSourceSnapshot>>>> = OnceLock::new();
static RELATED_SOURCE_CACHE: OnceLock<Mutex<Vec<Weak<SourceSnapshotRegistry>>>> = OnceLock::new();

fn engine_stage(phase: DiagnosticPhase) -> EngineDiagnosticStage {
    match phase {
        DiagnosticPhase::Lex | DiagnosticPhase::Parse => EngineDiagnosticStage::Parse,
        DiagnosticPhase::Resolve | DiagnosticPhase::TypeCheck => EngineDiagnosticStage::TypeCheck,
        DiagnosticPhase::Lower => EngineDiagnosticStage::Compile,
        DiagnosticPhase::Verify => EngineDiagnosticStage::Verify,
    }
}

fn analysis_stage(code: DiagnosticCode, fallback: EngineDiagnosticStage) -> EngineDiagnosticStage {
    match code.as_str() {
        "NX1001" | "NX1002" => EngineDiagnosticStage::Parse,
        "NX3001" | "NX3002" | "NX3003" | "NX3004" => EngineDiagnosticStage::Verify,
        "NX6005" => EngineDiagnosticStage::Compile,
        _ => fallback,
    }
}

fn shared_source_snapshot(
    package_id: Option<&PackageId>,
    files: &SourceFileRegistry,
) -> Arc<EngineSourceSnapshot> {
    let cache = SOURCE_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|snapshot| snapshot.strong_count() > 0);
    if let Some(snapshot) = cache
        .iter()
        .filter_map(Weak::upgrade)
        .find(|snapshot| source_snapshot_matches(snapshot, package_id, files))
    {
        return snapshot;
    }

    let mut builder = SourceSnapshotRegistry::builder();
    let mut identities_by_file = BTreeMap::new();
    for file in files.files() {
        let identity = package_id.map_or_else(
            || SourceIdentity::standalone(Arc::<str>::from(file.path.as_str())),
            |package_id| {
                SourceIdentity::package(
                    Arc::<str>::from(package_id.as_str()),
                    Arc::<str>::from(file.path.as_str()),
                )
            },
        );
        builder
            .insert(identity.clone(), Arc::<str>::from(file.text.as_str()))
            .expect("SourceFileRegistry paths are unique");
        identities_by_file.insert(file.id, identity);
    }
    let snapshot = Arc::new(EngineSourceSnapshot {
        sources: builder.build(),
        identities_by_file,
    });
    cache.push(Arc::downgrade(&snapshot));
    snapshot
}

fn shared_package_source_snapshot(files: &PackageSourceSnapshot) -> Arc<EngineSourceSnapshot> {
    let identities_by_file = files
        .files()
        .iter()
        .map(|source| (source.file, source.identity.clone()))
        .collect::<BTreeMap<_, _>>();
    let cache = SOURCE_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|snapshot| snapshot.strong_count() > 0);
    if let Some(snapshot) = cache.iter().filter_map(Weak::upgrade).find(|snapshot| {
        Arc::ptr_eq(&snapshot.sources, files.diagnostic_sources())
            && snapshot.identities_by_file == identities_by_file
    }) {
        return snapshot;
    }

    let snapshot = Arc::new(EngineSourceSnapshot {
        sources: Arc::clone(files.diagnostic_sources()),
        identities_by_file,
    });
    cache.push(Arc::downgrade(&snapshot));
    snapshot
}

fn shared_diagnostic_source_snapshot(
    sources: &Arc<SourceSnapshotRegistry>,
) -> Arc<EngineSourceSnapshot> {
    let identities_by_file = sources
        .iter()
        .enumerate()
        .map(|(index, (identity, _))| {
            let raw = u32::try_from(index)
                .expect("diagnostic source registry exceeds u32")
                .checked_add(1)
                .expect("diagnostic FileId exceeds u32");
            (FileId(raw), identity.clone())
        })
        .collect::<BTreeMap<_, _>>();
    let cache = SOURCE_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|snapshot| snapshot.strong_count() > 0);
    if let Some(snapshot) = cache.iter().filter_map(Weak::upgrade).find(|snapshot| {
        Arc::ptr_eq(&snapshot.sources, sources) && snapshot.identities_by_file == identities_by_file
    }) {
        return snapshot;
    }

    let snapshot = Arc::new(EngineSourceSnapshot {
        sources: Arc::clone(sources),
        identities_by_file,
    });
    cache.push(Arc::downgrade(&snapshot));
    snapshot
}

fn source_snapshot_matches(
    snapshot: &EngineSourceSnapshot,
    package_id: Option<&PackageId>,
    files: &SourceFileRegistry,
) -> bool {
    snapshot.identities_by_file.len() == files.files().len()
        && files.files().all(|file| {
            let Some(identity) = snapshot.identity(file.id) else {
                return false;
            };
            identity.package_id() == package_id.map(PackageId::as_str)
                && identity.path() == file.path.as_str()
                && snapshot
                    .sources
                    .get(identity)
                    .is_some_and(|source| source.text() == file.text.as_str())
        })
}

fn shared_related_source(identity: &SourceIdentity, text: &str) -> Arc<SourceSnapshotRegistry> {
    let cache = RELATED_SOURCE_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.retain(|snapshot| snapshot.strong_count() > 0);
    if let Some(snapshot) = cache.iter().filter_map(Weak::upgrade).find(|snapshot| {
        snapshot.len() == 1
            && snapshot
                .get(identity)
                .is_some_and(|source| source.text() == text)
    }) {
        return snapshot;
    }

    let mut builder = SourceSnapshotRegistry::builder();
    builder
        .insert(identity.clone(), Arc::<str>::from(text))
        .expect("a fresh one-source registry cannot contain duplicates");
    let snapshot = builder.build();
    cache.push(Arc::downgrade(&snapshot));
    snapshot
}

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
    pub file: Option<SourceIdentity>,
    pub span: Option<SourceSpan>,
}

/// One immutable Engine source revision shared by every diagnostic produced from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineSourceSnapshot {
    sources: Arc<SourceSnapshotRegistry>,
    identities_by_file: BTreeMap<FileId, SourceIdentity>,
}

impl EngineSourceSnapshot {
    #[must_use]
    pub fn sources(&self) -> &Arc<SourceSnapshotRegistry> {
        &self.sources
    }

    #[must_use]
    pub fn identity(&self, file: FileId) -> Option<&SourceIdentity> {
        self.identities_by_file.get(&file)
    }

    #[must_use]
    pub fn file_id(&self, identity: &SourceIdentity) -> Option<FileId> {
        self.identities_by_file
            .iter()
            .find_map(|(file, candidate)| (candidate == identity).then_some(*file))
    }

    #[must_use]
    pub fn source(&self, file: FileId) -> Option<&Arc<SourceSnapshot>> {
        self.identity(file)
            .and_then(|identity| self.sources.get(identity))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineDiagnostic {
    pub sequence: u64,
    pub package_id: Option<PackageId>,
    pub source_id: Option<SourceId>,
    pub stage: EngineDiagnosticStage,
    pub diagnostic: Diagnostic,
    pub file: Option<SourceIdentity>,
    pub related: Vec<RelatedDiagnostic>,
    pub source_snapshot: Option<Arc<EngineSourceSnapshot>>,
    pub related_source_snapshots: Vec<Arc<SourceSnapshotRegistry>>,
    pub context: EngineDiagnosticContext,
    /// Canonical source edits retained losslessly for tooling.
    pub edit_suggestions: Vec<TextEditSuggestion>,
    /// Compatibility-only textual fixes produced by legacy Engine stages.
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
            source_snapshot: None,
            related_source_snapshots: Vec::new(),
            context: EngineDiagnosticContext::default(),
            edit_suggestions: Vec::new(),
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
        let source_snapshot =
            sources.map(|sources| shared_source_snapshot(package_id.as_ref(), sources));
        Self::from_leaf_snapshot(package_id, source_id, stage, diagnostic, source_snapshot)
    }

    /// Creates an Engine diagnostic from the canonical compiler artifact snapshot.
    ///
    /// This preserves package-qualified identities across the complete linked dependency closure.
    #[must_use]
    pub fn from_package_snapshot(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        diagnostic: Diagnostic,
        sources: &PackageSourceSnapshot,
    ) -> Self {
        Self::from_leaf_snapshot(
            package_id,
            source_id,
            stage,
            diagnostic,
            Some(shared_package_source_snapshot(sources)),
        )
    }

    /// Adapts one canonical leaf diagnostic and its immutable source registry.
    ///
    /// A bare registry has no artifact `FileId` table, so numeric IDs are assigned densely in stable
    /// source-identity order. Use [`Self::from_package_leaf_diagnostic`] when exact compiler IDs
    /// are available.
    #[must_use]
    pub fn from_leaf_diagnostic(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        diagnostic: &LeafDiagnostic,
        sources: &Arc<SourceSnapshotRegistry>,
    ) -> Self {
        Self::from_canonical_leaf_snapshot(
            package_id,
            source_id,
            stage,
            diagnostic,
            shared_diagnostic_source_snapshot(sources),
        )
    }

    /// Adapts one canonical leaf diagnostic using exact compiler-assigned `FileId`s.
    #[must_use]
    pub fn from_package_leaf_diagnostic(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        diagnostic: &LeafDiagnostic,
        sources: &PackageSourceSnapshot,
    ) -> Self {
        Self::from_canonical_leaf_snapshot(
            package_id,
            source_id,
            stage,
            diagnostic,
            shared_package_source_snapshot(sources),
        )
    }

    /// Converts a deterministic analysis batch without cloning its source texts per diagnostic.
    ///
    /// Batch-only numeric IDs are dense and deterministic in source-identity order. They are not
    /// persisted identities and must not be confused with compiler artifact `FileId`s.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn from_diagnostic_batch(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        batch: &DiagnosticBatch,
    ) -> Vec<Self> {
        let source_snapshot = shared_diagnostic_source_snapshot(batch.sources());
        batch
            .diagnostics()
            .iter()
            .map(|diagnostic| {
                Self::from_canonical_leaf_snapshot(
                    package_id.clone(),
                    source_id.clone(),
                    analysis_stage(diagnostic.code, stage),
                    diagnostic,
                    Arc::clone(&source_snapshot),
                )
            })
            .collect()
    }

    #[allow(clippy::needless_pass_by_value)]
    fn from_canonical_leaf_snapshot(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        diagnostic: &LeafDiagnostic,
        source_snapshot: Arc<EngineSourceSnapshot>,
    ) -> Self {
        let facade = facade_diagnostic_from_leaf(diagnostic, &source_snapshot);
        let mut engine = Self::from_leaf_snapshot(
            package_id,
            source_id,
            stage,
            facade,
            Some(Arc::clone(&source_snapshot)),
        );
        engine.file = diagnostic.primary_label().map(|label| label.source.clone());
        engine.related = diagnostic
            .labels
            .iter()
            .filter(|label| label.style == LabelStyle::Secondary)
            .map(|label| RelatedDiagnostic {
                message: label.message.to_string(),
                file: Some(label.source.clone()),
                span: Some(SourceSpan::new(
                    source_snapshot.file_id(&label.source).unwrap_or_default(),
                    label.range.start,
                    label.range.end,
                )),
            })
            .chain(diagnostic.related.iter().map(|related| RelatedDiagnostic {
                message: related.message.to_string(),
                file: Some(related.source.clone()),
                span: Some(SourceSpan::new(
                    source_snapshot.file_id(&related.source).unwrap_or_default(),
                    related.range.start,
                    related.range.end,
                )),
            }))
            .collect();
        engine.edit_suggestions.clone_from(&diagnostic.fixes);
        engine
    }

    fn from_leaf_snapshot(
        package_id: Option<PackageId>,
        source_id: Option<SourceId>,
        stage: EngineDiagnosticStage,
        diagnostic: Diagnostic,
        source_snapshot: Option<Arc<EngineSourceSnapshot>>,
    ) -> Self {
        let stage = diagnostic.phase().map_or(stage, engine_stage);
        let file = diagnostic
            .primary
            .as_ref()
            .and_then(|label| source_snapshot.as_ref()?.identity(label.span.file))
            .cloned();
        let related = diagnostic
            .secondary
            .iter()
            .map(|label| RelatedDiagnostic {
                message: label.message.to_string(),
                file: source_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.identity(label.span.file))
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
            source_snapshot,
            related_source_snapshots: Vec::new(),
            context: EngineDiagnosticContext::default(),
            edit_suggestions: Vec::new(),
            fixes: Vec::new(),
        }
    }

    #[must_use]
    pub fn source_identity(&self, file: FileId) -> Option<&SourceIdentity> {
        self.source_snapshot.as_ref()?.identity(file)
    }

    #[must_use]
    pub fn source_by_identity(&self, identity: &SourceIdentity) -> Option<&SourceSnapshot> {
        self.source_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.sources.get(identity))
            .or_else(|| {
                self.related_source_snapshots
                    .iter()
                    .find_map(|sources| sources.get(identity))
            })
            .map(Arc::as_ref)
    }

    /// Attaches an immutable related source (for example a Host contract) without copying it into
    /// the package snapshot or into every related location.
    pub fn attach_related_source(
        &mut self,
        identity: SourceIdentity,
        text: impl AsRef<str>,
    ) -> SourceIdentity {
        if self.source_by_identity(&identity).is_none() {
            self.related_source_snapshots
                .push(shared_related_source(&identity, text.as_ref()));
        }
        identity
    }

    /// Returns the source-aware leaf representation used by shared renderers and tooling.
    #[must_use]
    pub fn leaf_diagnostic(&self) -> LeafDiagnostic {
        let secondary_count = self.diagnostic.secondary.len();
        let mut fallback_identities = VecDeque::new();
        if self.diagnostic.primary_source_identity().is_none()
            && let Some(primary) = &self.diagnostic.primary
        {
            fallback_identities.push_back(
                self.file
                    .clone()
                    .or_else(|| self.source_identity(primary.span.file).cloned())
                    .unwrap_or_else(|| {
                        SourceIdentity::standalone(format!("<file:{}>", primary.span.file.0))
                    }),
            );
        }
        for (index, secondary) in self.diagnostic.secondary.iter().enumerate() {
            if self.diagnostic.secondary_source_identity(index).is_some() {
                continue;
            }
            fallback_identities.push_back(
                self.related
                    .get(index)
                    .and_then(|related| related.file.clone())
                    .or_else(|| self.source_identity(secondary.span.file).cloned())
                    .unwrap_or_else(|| {
                        SourceIdentity::standalone(format!("<file:{}>", secondary.span.file.0))
                    }),
            );
        }
        let mut leaf = self.diagnostic.to_leaf_with_source_identities(|file| {
            fallback_identities
                .pop_front()
                .unwrap_or_else(|| SourceIdentity::standalone(format!("<file:{}>", file.0)))
        });
        leaf.related.extend(
            self.related
                .iter()
                .skip(secondary_count)
                .filter_map(|related| {
                    let identity = related.file.clone()?;
                    let span = related.span?;
                    Some(LeafRelatedLocation::new(
                        identity,
                        ByteRange::new(span.start, span.end),
                        related.message.clone(),
                    ))
                }),
        );
        leaf.fixes.extend(self.edit_suggestions.iter().cloned());
        leaf.fixes
            .extend(self.fixes.iter().cloned().map(TextEditSuggestion::message));
        leaf
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
            file: self.file.clone(),
            range: self
                .diagnostic
                .primary
                .as_ref()
                .and_then(|label| source_range(self, self.file.as_ref(), label.span)),
        }
    }
}

fn facade_diagnostic_from_leaf(
    diagnostic: &LeafDiagnostic,
    sources: &EngineSourceSnapshot,
) -> Diagnostic {
    let mut facade = Diagnostic::without_source(
        diagnostic.code,
        diagnostic.severity,
        RuntimeMessage::inline(&diagnostic.message),
    );
    for label in &diagnostic.labels {
        let facade_label = nexa::Label {
            span: SourceSpan::new(
                sources.file_id(&label.source).unwrap_or_default(),
                label.range.start,
                label.range.end,
            ),
            message: RuntimeMessage::inline(&label.message),
        };
        if facade.primary.is_none() && label.style == LabelStyle::Primary {
            facade.primary = Some(facade_label);
        } else {
            facade.secondary.push(facade_label);
        }
    }
    facade.notes.extend(
        diagnostic
            .notes
            .iter()
            .map(|note| RuntimeMessage::inline(note)),
    );
    facade
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineDiagnosticSummary {
    pub sequence: u64,
    pub package_id: Option<PackageId>,
    pub stage: EngineDiagnosticStage,
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub message: String,
    pub file: Option<SourceIdentity>,
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
            if let Some(source) = source_snapshot(diagnostic, file) {
                let position = source.human_position(label.span.start as usize);
                write!(output, "\n{}:{}:{}", file, position.line, position.column)
                    .expect("String writes do not fail");
                if let Some(source_line) = source.line_text(position.line) {
                    write!(output, "\n  {source_line}").expect("String writes do not fail");
                }
            } else {
                write!(output, "\n{file}:<source unavailable>").expect("String writes do not fail");
            }
            write!(output, "\n  {}", label.message).expect("String writes do not fail");
        }
        for related in &diagnostic.related {
            write!(output, "\nrelated: {}", related.message).expect("String writes do not fail");
            if let (Some(file), Some(span)) = (&related.file, related.span) {
                if let Some(source) = source_snapshot(diagnostic, file) {
                    let position = source.human_position(span.start as usize);
                    write!(output, " at {}:{}:{}", file, position.line, position.column)
                        .expect("String writes do not fail");
                } else {
                    write!(output, " at {file}:<source unavailable>")
                        .expect("String writes do not fail");
                }
            }
        }
        for note in &diagnostic.diagnostic.notes {
            write!(output, "\nnote: {note}").expect("String writes do not fail");
        }
        for fix in &diagnostic.fixes {
            write!(output, "\nhelp: {}", bounded_message(fix)).expect("String writes do not fail");
        }
        for fix in &diagnostic.edit_suggestions {
            write!(output, "\nhelp: {}", bounded_message(&fix.message))
                .expect("String writes do not fail");
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
    position_encoding: &'static str,
    sequence: u64,
    code: &'static str,
    severity: &'static str,
    stage: EngineDiagnosticStage,
    package_id: Option<String>,
    source_id: Option<String>,
    file: Option<String>,
    source_identity: Option<String>,
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
#[serde(rename_all = "camelCase")]
struct RelatedOutput {
    message: String,
    file: Option<String>,
    source_identity: Option<String>,
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
            .and_then(|label| source_range(engine, engine.file.as_ref(), label.span))
            .map(RangeOutput::from);
        Self {
            schema: 1,
            position_encoding: MACHINE_POSITION_ENCODING,
            sequence: engine.sequence,
            code: engine.diagnostic.code.as_str(),
            severity: engine.diagnostic.severity.as_str(),
            stage: engine.stage,
            package_id: engine.package_id.as_ref().map(ToString::to_string),
            source_id: engine.source_id.as_ref().map(ToString::to_string),
            file: engine.file.as_ref().map(|file| file.path().to_owned()),
            source_identity: engine.file.as_ref().map(ToString::to_string),
            range,
            message: bounded_message(&engine.diagnostic.message.to_string()),
            related: engine
                .related
                .iter()
                .map(|related| RelatedOutput {
                    message: bounded_message(&related.message),
                    file: related.file.as_ref().map(|file| file.path().to_owned()),
                    source_identity: related.file.as_ref().map(ToString::to_string),
                    range: related
                        .span
                        .and_then(|span| source_range(engine, related.file.as_ref(), span))
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
                .chain(
                    engine
                        .edit_suggestions
                        .iter()
                        .map(|fix| bounded_message(&fix.message)),
                )
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

fn source_snapshot<'a>(
    diagnostic: &'a EngineDiagnostic,
    identity: &SourceIdentity,
) -> Option<&'a SourceSnapshot> {
    diagnostic.source_by_identity(identity)
}

fn source_range(
    diagnostic: &EngineDiagnostic,
    identity: Option<&SourceIdentity>,
    span: SourceSpan,
) -> Option<SourceRange> {
    let identity = identity?;
    let range =
        source_snapshot(diagnostic, identity)?.utf16_range(ByteRange::new(span.start, span.end));
    Some(SourceRange {
        start: SourcePosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: SourcePosition {
            line: range.end.line,
            character: range.end.character,
        },
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexa::FileId;
    use nexa::prelude::SourceSpan;
    use nexa::{
        ByteRange, CompiledSource, DiagnosticBatch, ErrorCode, LeafDiagnostic, LeafLabel,
        LeafRelatedLocation, PackageSourceSnapshot, Severity, SourceIdentity, SourceSnapshot,
        SourceSnapshotRegistry, TextEditSuggestion,
    };
    use nexa_analysis::{NormalizedPackagePath, PackageId as AnalysisPackageId, SourceKey};

    use super::{
        DiagnosticRenderer, EngineDiagnostic, EngineDiagnosticStage, PackageId, RelatedDiagnostic,
        SourceFileRegistry,
    };

    #[test]
    fn cross_file_crlf_and_astral_ranges_use_one_shared_snapshot() {
        let first_source = "pub 😀x\r\n";
        let duplicate_source = "head\r\n😀tail";
        let registry = SourceFileRegistry::from_files([
            ("a.nexa", first_source),
            ("b.nexa", duplicate_source),
        ])
        .unwrap();
        let first_file = registry.file_id("a.nexa").unwrap();
        let duplicate_file = registry.file_id("b.nexa").unwrap();
        let mut leaf = nexa::Diagnostic::from_parts(
            ErrorCode::NX2001,
            Severity::Error,
            nexa::RuntimeMessage::inline("duplicate name `😀`"),
            nexa::Label {
                span: SourceSpan::new(duplicate_file, 6, 10),
                message: nexa::RuntimeMessage::Static("primary source location"),
            },
        );
        leaf.secondary.push(nexa::Label {
            span: SourceSpan::new(first_file, 4, 8),
            message: nexa::RuntimeMessage::Static("first declaration"),
        });
        let package_id = PackageId::new("example.app").unwrap();
        let mut diagnostic = EngineDiagnostic::from_leaf(
            Some(package_id.clone()),
            None,
            EngineDiagnosticStage::TypeCheck,
            leaf.clone(),
            Some(&registry),
        );
        let second = EngineDiagnostic::from_leaf(
            Some(package_id),
            None,
            EngineDiagnosticStage::TypeCheck,
            leaf,
            Some(&registry),
        );

        assert_eq!(diagnostic.stage, EngineDiagnosticStage::TypeCheck);
        assert_eq!(
            diagnostic.file.as_ref().map(SourceIdentity::path),
            Some("b.nexa")
        );
        assert_eq!(
            diagnostic.related[0]
                .file
                .as_ref()
                .map(SourceIdentity::path),
            Some("a.nexa")
        );
        assert!(Arc::ptr_eq(
            diagnostic.source_snapshot.as_ref().unwrap(),
            second.source_snapshot.as_ref().unwrap()
        ));
        let contract = SourceIdentity::standalone("host://contract.nidl");
        assert_eq!(
            diagnostic.attach_related_source(contract.clone(), "export fn tick();"),
            contract
        );
        diagnostic.attach_related_source(contract.clone(), "export fn tick();");
        assert_eq!(diagnostic.related_source_snapshots.len(), 1);
        assert_eq!(
            diagnostic
                .source_by_identity(&contract)
                .map(SourceSnapshot::text),
            Some("export fn tick();")
        );

        let leaf = diagnostic.leaf_diagnostic();
        assert_eq!(leaf.labels[0].source.path(), "b.nexa");
        assert_eq!(leaf.labels[1].source.path(), "a.nexa");

        let human = DiagnosticRenderer::human(&diagnostic);
        assert!(human.contains("example.app:b.nexa:2:1"));
        assert!(human.contains("related: first declaration at example.app:a.nexa:1:5"));

        let json: serde_json::Value =
            serde_json::from_str(&DiagnosticRenderer::json(&diagnostic).unwrap()).unwrap();
        assert_eq!(json["positionEncoding"], "utf-16-0-based");
        assert_eq!(json["range"]["start"]["line"], 1);
        assert_eq!(json["range"]["start"]["character"], 0);
        assert_eq!(json["range"]["end"]["character"], 2);
        assert_eq!(json["related"][0]["file"], "a.nexa");
        assert_eq!(json["related"][0]["range"]["start"]["line"], 0);
        assert_eq!(json["related"][0]["range"]["start"]["character"], 4);
        assert_eq!(json["related"][0]["range"]["end"]["character"], 6);
    }

    #[test]
    fn canonical_primary_does_not_shift_the_unresolved_secondary_identity() {
        let primary = SourceIdentity::standalone("host://primary.nidl");
        let secondary = SourceIdentity::standalone("host://secondary.nidl");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(primary.clone(), "interface Primary {}")
            .unwrap();
        sources
            .insert(secondary.clone(), "interface Secondary {}")
            .unwrap();
        let sources = sources.build();
        let leaf =
            LeafDiagnostic::new(ErrorCode::NX2101, Severity::Error, "mixed identities").with_label(
                LeafLabel::primary(primary.clone(), ByteRange::new(10, 17), "canonical primary"),
            );
        let mut engine = EngineDiagnostic::from_leaf_diagnostic(
            None,
            None,
            EngineDiagnosticStage::TypeCheck,
            &leaf,
            &sources,
        );
        let secondary_file = engine
            .source_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.file_id(&secondary))
            .unwrap();
        let secondary_span = SourceSpan::new(secondary_file, 10, 19);
        engine.diagnostic.secondary.push(nexa::Label {
            span: secondary_span,
            message: nexa::RuntimeMessage::Static("unresolved secondary"),
        });
        engine.related.push(RelatedDiagnostic {
            message: "unresolved secondary".into(),
            file: Some(secondary.clone()),
            span: Some(secondary_span),
        });

        let round_trip = engine.leaf_diagnostic();
        assert_eq!(round_trip.labels[0].source, primary);
        assert_eq!(round_trip.labels[1].source, secondary);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn package_adapter_preserves_file_ids_and_batch_adapter_shares_sources() {
        let dependency_key = SourceKey::new(
            AnalysisPackageId::new("dep.lib").unwrap(),
            NormalizedPackagePath::new("src/value.nexa").unwrap(),
        );
        let root_key = SourceKey::new(
            AnalysisPackageId::new("root.app").unwrap(),
            NormalizedPackagePath::new("src/main.nexa").unwrap(),
        );
        let dependency = SourceIdentity::package("dep.lib", "src/value.nexa");
        let root = SourceIdentity::package("root.app", "src/main.nexa");
        let contract = SourceIdentity::standalone("host://contract.nidl");
        let sources = PackageSourceSnapshot::new([
            CompiledSource {
                file: FileId(1),
                key: Some(dependency_key),
                identity: dependency.clone(),
                module_path: Some("value".into()),
                text: Arc::from("pub fn value() -> i32 { 1 }"),
                compiler_provided: false,
            },
            CompiledSource {
                file: FileId(2),
                key: Some(root_key),
                identity: root.clone(),
                module_path: Some("main".into()),
                text: Arc::from("fn main() { value(); }"),
                compiler_provided: false,
            },
            CompiledSource {
                file: FileId(3),
                key: None,
                identity: contract.clone(),
                module_path: None,
                text: Arc::from("export fn value() -> i32;"),
                compiler_provided: true,
            },
        ])
        .unwrap();
        let leaf = LeafDiagnostic::new(ErrorCode::NX2101, Severity::Error, "type mismatch")
            .with_label(LeafLabel::primary(
                root.clone(),
                ByteRange::new(12, 17),
                "invalid call",
            ))
            .with_label(LeafLabel::secondary(
                dependency.clone(),
                ByteRange::new(7, 12),
                "declaration",
            ))
            .with_related(LeafRelatedLocation::new(
                dependency.clone(),
                ByteRange::new(0, 12),
                "dependency definition",
            ))
            .with_related(LeafRelatedLocation::new(
                contract.clone(),
                ByteRange::new(10, 15),
                "host contract definition",
            ))
            .with_fix(TextEditSuggestion::replacement(
                "use the contract export",
                root.clone(),
                ByteRange::new(12, 17),
                "value",
            ));

        let engine = EngineDiagnostic::from_package_leaf_diagnostic(
            None,
            None,
            EngineDiagnosticStage::TypeCheck,
            &leaf,
            &sources,
        );
        assert_eq!(
            engine.diagnostic.primary.as_ref().unwrap().span.file,
            FileId(2)
        );
        assert_eq!(engine.file.as_ref(), Some(&root));
        assert_eq!(engine.related[0].file.as_ref(), Some(&dependency));
        assert_eq!(engine.related[0].span.unwrap().file, FileId(1));
        assert_eq!(engine.related[1].file.as_ref(), Some(&dependency));
        assert_eq!(engine.related[1].span.unwrap().file, FileId(1));
        assert_eq!(engine.related[2].file.as_ref(), Some(&contract));
        assert_eq!(engine.related[2].span.unwrap().file, FileId(3));
        assert_eq!(engine.edit_suggestions, leaf.fixes);
        assert_eq!(engine.leaf_diagnostic().fixes, leaf.fixes);
        assert_eq!(engine.summary().file.as_ref(), Some(&root));
        let rendered_human = DiagnosticRenderer::human(&engine);
        assert!(rendered_human.contains("root.app:src/main.nexa:1:13"));
        assert!(rendered_human.contains("dep.lib:src/value.nexa:1:8"));
        assert!(rendered_human.contains("host://contract.nidl:1:11"));
        let rendered: serde_json::Value =
            serde_json::from_str(&DiagnosticRenderer::json(&engine).unwrap()).unwrap();
        assert_eq!(rendered["sourceIdentity"], "root.app:src/main.nexa");
        assert_eq!(
            rendered["related"][2]["sourceIdentity"],
            "host://contract.nidl"
        );

        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::clone(sources.diagnostic_sources()));
        batch.push(leaf);
        let adapted = EngineDiagnostic::from_diagnostic_batch(
            None,
            None,
            EngineDiagnosticStage::TypeCheck,
            &batch,
        );
        assert_eq!(adapted.len(), 1);
        assert_eq!(
            adapted[0].diagnostic.primary.as_ref().unwrap().span.file,
            FileId(3)
        );
        assert!(Arc::ptr_eq(
            adapted[0].source_snapshot.as_ref().unwrap().sources(),
            sources.diagnostic_sources()
        ));
    }
}
