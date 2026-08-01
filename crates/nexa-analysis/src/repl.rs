use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use nexa_core::{CanonicalSymbolIdentity, StableId, StableSymbolId, SymbolKind};
use nexa_diagnostics::{
    ByteRange, Diagnostic, DiagnosticBatch, ErrorCode, Label, RelatedLocation, Severity,
    SourceIdentity,
};

use crate::{
    AnalysisEnvironment, AnalysisOutcome, ArtifactFileId, DeclarationVisibility, Definition,
    DefinitionId, DefinitionKind, IrEffect, IrType, LifecycleBindingsIr, ModulePath,
    NormalizedPackagePath, PackageId, PackageSemanticMetadata, QueryDatabase, ReplEntrypointIr,
    ResolvedBuildInput, SourceKey, SourceRange, StableSymbolIdentity, StateMetadataIr, StateTypeIr,
    TypedDeclarationBody, TypedDeclarationIr, TypedModuleIr, TypedPackageIr, TypedPlaceIr,
    TypedStatementIr, TypedTypeLayoutIr, canonical_state_schema, public_api_fingerprint,
};

/// The synthetic package used for every cumulative REPL session.
pub const REPL_PACKAGE_ID: &str = "nexa.repl";
/// The synthetic module used for every cumulative REPL session.
pub const REPL_MODULE_PATH: &str = "repl.session";
/// The synthetic `@state class` that owns persisted top-level value bindings.
pub const REPL_ENVIRONMENT_TYPE_NAME: &str = "__ReplEnvironment";
/// Initial ABI version of the reserved hidden session state class.
pub const REPL_ENVIRONMENT_STATE_VERSION: u32 = 1;
/// Cell ordinals start at one. Failed cells may leave gaps, but committed ordinals always increase.
pub const REPL_FIRST_CELL_ORDINAL: u64 = 1;

/// One immutable REPL submission.
///
/// The source remains a distinct snapshot. A session never concatenates it with earlier source
/// text; earlier declarations are made available through [`ReplSessionSnapshot`] instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplCellInput {
    pub ordinal: u64,
    pub source: SourceIdentity,
    pub text: Arc<str>,
}

impl ReplCellInput {
    #[must_use]
    pub fn new(ordinal: u64, source: SourceIdentity, text: impl Into<Arc<str>>) -> Self {
        Self {
            ordinal,
            source,
            text: text.into(),
        }
    }
}

/// The namespace and callable metadata of a REPL binding.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplBindingKind {
    /// A top-level `let` or `let mut`, persisted in a hidden state slot.
    Value,
    /// A top-level function. `ty` on the enclosing slot is its result type.
    Function {
        parameters: Arc<[IrType]>,
        effect: IrEffect,
    },
}

/// A binding discovered while analyzing one cell, before a stable session slot is assigned.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplBindingDeclaration {
    pub name: String,
    pub definition: DefinitionId,
    /// Analyzer-assigned identity of the concrete declaration occurrence.
    ///
    /// Hand-built session tests may omit this and use the deterministic declaration-index
    /// fallback in [`ReplAnalysisSession::stage_cell`].
    pub stable_symbol: Option<StableSymbolId>,
    pub kind: ReplBindingKind,
    pub mutable: bool,
    /// The value type for a value binding, or result type for a function binding.
    pub ty: IrType,
    pub span: SourceRange,
}

impl ReplBindingDeclaration {
    #[must_use]
    pub fn value(
        name: impl Into<String>,
        definition: DefinitionId,
        mutable: bool,
        ty: IrType,
        span: SourceRange,
    ) -> Self {
        Self {
            name: name.into(),
            definition,
            stable_symbol: None,
            kind: ReplBindingKind::Value,
            mutable,
            ty,
            span,
        }
    }

    #[must_use]
    pub fn function(
        name: impl Into<String>,
        definition: DefinitionId,
        parameters: impl Into<Arc<[IrType]>>,
        result: IrType,
        effect: IrEffect,
        span: SourceRange,
    ) -> Self {
        Self {
            name: name.into(),
            definition,
            stable_symbol: None,
            kind: ReplBindingKind::Function {
                parameters: parameters.into(),
                effect,
            },
            mutable: false,
            ty: result,
            span,
        }
    }

    #[must_use]
    pub fn with_stable_symbol(mut self, stable_symbol: StableSymbolId) -> Self {
        self.stable_symbol = Some(stable_symbol);
        self
    }
}

/// A nominal type discovered while analyzing one cell.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplTypeDeclaration {
    pub name: String,
    pub definition: DefinitionId,
    pub ty: IrType,
    pub span: SourceRange,
}

impl ReplTypeDeclaration {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        definition: DefinitionId,
        ty: IrType,
        span: SourceRange,
    ) -> Self {
        Self {
            name: name.into(),
            definition,
            ty,
            span,
        }
    }
}

/// The structured semantic delta produced by successful analysis of one cell.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplCellDelta {
    pub bindings: Vec<ReplBindingDeclaration>,
    pub types: Vec<ReplTypeDeclaration>,
    pub entry: ReplEntrypointIr,
    pub entry_span: SourceRange,
}

impl ReplCellDelta {
    #[must_use]
    pub fn new(
        bindings: Vec<ReplBindingDeclaration>,
        types: Vec<ReplTypeDeclaration>,
        entry: ReplEntrypointIr,
        entry_span: SourceRange,
    ) -> Self {
        Self {
            bindings,
            types,
            entry,
            entry_span,
        }
    }
}

/// The result of analyzing a cell before it is offered to a session transaction.
#[derive(Clone, Debug)]
pub enum ReplCellAnalysisOutcome {
    Accepted(ReplCellDelta),
    Rejected(DiagnosticBatch),
}

/// Exact inputs for cumulative analysis of one REPL cell.
///
/// `build_input` contains only the staged cell source. The prior semantic environment comes from
/// `snapshot`; callers must not materialize it by concatenating earlier source text.
/// `current_source` is compiler-facing authority and is deliberately separate from the
/// reader-facing [`ReplCellInput::source`].
#[derive(Debug)]
pub struct ReplSessionInput<'a> {
    pub snapshot: &'a ReplSessionSnapshot,
    pub cell: ReplCellInput,
    pub current_source: SourceKey,
    pub build_input: &'a ResolvedBuildInput,
}

impl<'a> ReplSessionInput<'a> {
    #[must_use]
    pub fn new(
        snapshot: &'a ReplSessionSnapshot,
        cell: ReplCellInput,
        current_source: SourceKey,
        build_input: &'a ResolvedBuildInput,
    ) -> Self {
        Self {
            snapshot,
            cell,
            current_source,
            build_input,
        }
    }
}

/// Formal analyzer result for a cumulative cell.
///
/// The rejected variant cannot carry a [`ReplStagedCell`], making compile failure impossible to
/// commit through this API.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ReplSessionAnalysisOutcome {
    Accepted {
        analysis: AnalysisOutcome,
        delta: ReplCellDelta,
        staged: ReplStagedCell,
        latest_entry: StableSymbolId,
    },
    Rejected {
        analysis: AnalysisOutcome,
    },
}

impl ReplSessionAnalysisOutcome {
    #[must_use]
    pub fn analysis(&self) -> &AnalysisOutcome {
        match self {
            Self::Accepted { analysis, .. } | Self::Rejected { analysis } => analysis,
        }
    }

    #[must_use]
    pub fn latest_entry(&self) -> Option<StableSymbolId> {
        match self {
            Self::Accepted { latest_entry, .. } => Some(*latest_entry),
            Self::Rejected { .. } => None,
        }
    }

    #[must_use]
    pub fn into_staged(self) -> Option<ReplStagedCell> {
        match self {
            Self::Accepted { staged, .. } => Some(staged),
            Self::Rejected { .. } => None,
        }
    }
}

/// Analyze and transactionally stage one cumulative REPL cell.
///
/// The returned `AnalysisOutcome.ir`, when accepted, is the candidate attached to the stage token.
/// A rejected analysis never exposes a stage token.
#[must_use]
pub fn analyze_repl_session_cell(
    input: &ReplSessionInput<'_>,
    environment: &AnalysisEnvironment,
    db: &mut QueryDatabase,
) -> ReplSessionAnalysisOutcome {
    let mut analysis = crate::analyzer::analyze_repl_cell_with_session(
        input.build_input,
        input.snapshot,
        &input.cell,
        environment,
        db,
    );
    let Some(ir) = analysis.ir.as_ref() else {
        return ReplSessionAnalysisOutcome::Rejected { analysis };
    };
    // The internal source key and reader-facing origin intentionally need not have the same path.
    // Package identity and exact bytes still have to agree, and the supplied key must select the
    // one production unit captured by the resolved build.
    let source_is_exact = input.cell.source.package_id()
        == Some(input.current_source.package_id.as_str())
        && input
            .build_input
            .root_source_set
            .production_units()
            .any(|unit| {
                unit.key == input.current_source && unit.text.as_ref() == input.cell.text.as_ref()
            });
    if !source_is_exact {
        reject_repl_adapter(
            &mut analysis,
            &input.cell,
            "resolved REPL input does not contain the exact internal cell source and text",
        );
        return ReplSessionAnalysisOutcome::Rejected { analysis };
    }
    let delta = match repl_delta_from_ir(ir, &input.current_source) {
        Ok(delta) => delta,
        Err(message) => {
            reject_repl_adapter(&mut analysis, &input.cell, message);
            return ReplSessionAnalysisOutcome::Rejected { analysis };
        }
    };
    let candidate_ir = Arc::new(ir.clone());
    let session = ReplAnalysisSession::from_snapshot(input.snapshot.clone());
    let staging = session.stage_cell(
        input.cell.clone(),
        ReplCellAnalysisOutcome::Accepted(delta.clone()),
    );
    let mut staged = match staging {
        Ok(ReplStagingOutcome::Ready(staged)) => staged,
        Ok(ReplStagingOutcome::Rejected(_)) => {
            reject_repl_adapter(
                &mut analysis,
                &input.cell,
                "accepted analysis unexpectedly produced a rejected REPL stage",
            );
            return ReplSessionAnalysisOutcome::Rejected { analysis };
        }
        Err(error) => {
            push_repl_session_error(&mut analysis.diagnostics, &input.cell, &error);
            analysis.ir = None;
            return ReplSessionAnalysisOutcome::Rejected { analysis };
        }
    };
    staged.attach_candidate_ir(candidate_ir);
    let latest_entry = staged.entry_symbol();
    ReplSessionAnalysisOutcome::Accepted {
        analysis,
        delta,
        staged,
        latest_entry,
    }
}

#[allow(clippy::too_many_lines)]
fn repl_delta_from_ir(
    ir: &TypedPackageIr,
    current_source: &SourceKey,
) -> Result<ReplCellDelta, String> {
    let entry =
        ir.metadata().repl_entry.clone().ok_or_else(|| {
            "typed REPL candidate has no authoritative cell entrypoint".to_owned()
        })?;
    let entry_definition = ir
        .definition(entry.function)
        .ok_or_else(|| "typed REPL entrypoint definition is absent".to_owned())?;
    let mut bindings = Vec::new();
    let mut types = Vec::new();
    let field_mutability = ir
        .modules()
        .iter()
        .flat_map(|module| module.declarations.iter())
        .filter_map(|declaration| match &declaration.body {
            TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Class { fields, .. }) => {
                Some(fields.iter())
            }
            _ => None,
        })
        .flatten()
        .map(|field| (field.definition, field.mutable))
        .collect::<BTreeMap<_, _>>();
    let mut seen_value_bindings = BTreeSet::new();

    for module in ir.modules() {
        for declaration in module.declarations.iter() {
            let definition = ir
                .definition(declaration.definition)
                .ok_or_else(|| "typed REPL declaration has an invalid DefinitionId".to_owned())?;
            if declaration.definition == entry.function {
                let TypedDeclarationBody::Function(function) = &declaration.body else {
                    return Err("typed REPL entrypoint is not a function".to_owned());
                };
                for statement in &function.body.statements {
                    let (definition, mutable) = match statement {
                        TypedStatementIr::Let {
                            definition,
                            mutable,
                            ..
                        } => (*definition, *mutable),
                        TypedStatementIr::Assign {
                            target: TypedPlaceIr::StateField { field, .. },
                            ..
                        } => (
                            *field,
                            field_mutability.get(field).copied().unwrap_or(false),
                        ),
                        _ => continue,
                    };
                    let binding = ir.definition(definition).ok_or_else(|| {
                        "typed REPL value binding has an invalid DefinitionId".to_owned()
                    })?;
                    if binding.span.source != *current_source
                        || !seen_value_bindings.insert(definition)
                    {
                        continue;
                    }
                    let mut declaration = ReplBindingDeclaration::value(
                        binding.name.clone(),
                        definition,
                        mutable,
                        binding.ty.clone(),
                        binding.span.clone(),
                    );
                    if let Some(stable) = &binding.stable_symbol {
                        declaration = declaration.with_stable_symbol(stable.runtime_id);
                    }
                    bindings.push(declaration);
                }
                continue;
            }
            if definition.span.source != *current_source {
                continue;
            }
            match &declaration.body {
                TypedDeclarationBody::Function(function)
                    if matches!(
                        definition.kind,
                        DefinitionKind::Function | DefinitionKind::Task
                    ) && !definition.name.starts_with("__defer_") =>
                {
                    let parameters = function
                        .parameters
                        .iter()
                        .map(|parameter| {
                            ir.definition(*parameter)
                                .map(|definition| definition.ty.clone())
                                .ok_or_else(|| {
                                    "typed REPL function parameter has an invalid DefinitionId"
                                        .to_owned()
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut binding = ReplBindingDeclaration::function(
                        definition.name.clone(),
                        declaration.definition,
                        parameters,
                        function.return_type.clone(),
                        function.effect,
                        definition.span.clone(),
                    );
                    if let Some(stable) = &definition.stable_symbol {
                        binding = binding.with_stable_symbol(stable.runtime_id);
                    }
                    bindings.push(binding);
                }
                TypedDeclarationBody::TypeLayout(_) => {
                    types.push(ReplTypeDeclaration::new(
                        definition.name.clone(),
                        declaration.definition,
                        definition.ty.clone(),
                        definition.span.clone(),
                    ));
                }
                TypedDeclarationBody::Const(_)
                | TypedDeclarationBody::External
                | TypedDeclarationBody::Function(_) => {}
            }
        }
    }

    Ok(ReplCellDelta::new(
        bindings,
        types,
        entry,
        entry_definition.span.clone(),
    ))
}

fn reject_repl_adapter(
    analysis: &mut AnalysisOutcome,
    cell: &ReplCellInput,
    message: impl Into<Arc<str>>,
) {
    analysis.diagnostics.push(
        Diagnostic::new(ErrorCode::NX2101, Severity::Error, message).with_label(Label::primary(
            cell.source.clone(),
            ByteRange::new(0, u32::try_from(cell.text.len()).unwrap_or(u32::MAX)),
            "REPL cell cannot be staged",
        )),
    );
    analysis.ir = None;
}

fn push_repl_session_error(
    diagnostics: &mut DiagnosticBatch,
    cell: &ReplCellInput,
    error: &ReplSessionError,
) {
    let diagnostic = match error {
        ReplSessionError::TypeAlreadyDefined {
            name,
            original,
            attempted,
            ..
        } => Diagnostic::new(
            ErrorCode::NX2704,
            Severity::Error,
            format!("REPL type `{name}` is already defined"),
        )
        .with_label(Label::primary(
            source_identity_from_key(&attempted.source),
            source_range_bytes(attempted),
            "type redefinition is not allowed in a REPL session",
        ))
        .with_related(RelatedLocation::new(
            source_identity_from_key(&original.source),
            source_range_bytes(original),
            "original type declaration",
        ))
        .with_note("use `:reset` to begin a new type registry"),
        _ => Diagnostic::new(ErrorCode::NX2101, Severity::Error, error.to_string()).with_label(
            Label::primary(
                cell.source.clone(),
                ByteRange::new(0, u32::try_from(cell.text.len()).unwrap_or(u32::MAX)),
                "REPL session invariant rejected this cell",
            ),
        ),
    };
    diagnostics.push(diagnostic);
}

fn source_identity_from_key(source: &SourceKey) -> SourceIdentity {
    SourceIdentity::package(source.package_id.as_str(), source.path.as_str())
}

fn source_range_bytes(range: &SourceRange) -> ByteRange {
    ByteRange::new(range.start, range.end)
}

/// A persistent value/function slot.
///
/// Value slots retain a state-field identity even after a later cell shadows their source name.
/// Functions do not have state storage and therefore have `state_slot == None`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplBindingSlot {
    pub name: String,
    pub definition: DefinitionId,
    pub stable_symbol: StableSymbolId,
    pub state_slot: Option<StableId>,
    pub kind: ReplBindingKind,
    pub mutable: bool,
    pub ty: IrType,
    pub defining_cell: u64,
    pub span: SourceRange,
}

/// A nominal type registered by a committed cell.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplTypeSlot {
    pub name: String,
    pub definition: DefinitionId,
    pub stable_symbol: StableSymbolId,
    pub ty: IrType,
    pub defining_cell: u64,
    pub span: SourceRange,
}

/// One field in the structured hidden `@state class __ReplEnvironment`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplStateSlot {
    pub name: String,
    pub definition: DefinitionId,
    pub stable_id: StableId,
    pub binding_symbol: StableSymbolId,
    pub mutable: bool,
    pub ty: IrType,
    pub defining_cell: u64,
    pub span: SourceRange,
}

/// Structured state-class metadata consumed by reload/runtime integration.
///
/// This is deliberately an IR data structure rather than synthesized Nexa source.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplEnvironmentLayout {
    pub class_symbol: StableSymbolId,
    pub slots: Arc<[ReplStateSlot]>,
}

/// Metadata retained for one committed cell.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplCommittedCell {
    pub input: ReplCellInput,
    pub entry_symbol: StableSymbolId,
    pub entry_function: DefinitionId,
    pub entry_span: SourceRange,
    pub entry_effect: IrEffect,
    pub result_type: IrType,
    pub binding_symbols: Arc<[StableSymbolId]>,
    pub type_symbols: Arc<[StableSymbolId]>,
}

impl ReplCommittedCell {
    #[must_use]
    pub fn ordinal(&self) -> u64 {
        self.input.ordinal
    }
}

/// An immutable, cheaply cloned view of all committed REPL semantics.
#[derive(Clone, Debug)]
pub struct ReplSessionSnapshot {
    package_id: PackageId,
    module: ModulePath,
    revision: u64,
    committed_cells: Arc<[ReplCommittedCell]>,
    binding_slots: Arc<[ReplBindingSlot]>,
    visible_bindings: BTreeMap<String, usize>,
    type_registry: BTreeMap<String, ReplTypeSlot>,
    environment: ReplEnvironmentLayout,
    latest_entry: Option<StableSymbolId>,
    candidate_ir: Option<Arc<TypedPackageIr>>,
}

impl ReplSessionSnapshot {
    #[must_use]
    pub fn package_id(&self) -> &PackageId {
        &self.package_id
    }

    #[must_use]
    pub fn module(&self) -> &ModulePath {
        &self.module
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn committed_cells(&self) -> &[ReplCommittedCell] {
        &self.committed_cells
    }

    #[must_use]
    pub fn last_committed_ordinal(&self) -> Option<u64> {
        self.committed_cells.last().map(ReplCommittedCell::ordinal)
    }

    /// All historical slots, including values/functions shadowed by later cells.
    #[must_use]
    pub fn binding_slots(&self) -> &[ReplBindingSlot] {
        &self.binding_slots
    }

    /// Resolves the binding visible to the next cell.
    #[must_use]
    pub fn visible_binding(&self, name: &str) -> Option<&ReplBindingSlot> {
        self.visible_bindings
            .get(name)
            .and_then(|index| self.binding_slots.get(*index))
    }

    #[must_use]
    pub fn visible_bindings(&self) -> impl ExactSizeIterator<Item = (&str, &ReplBindingSlot)> {
        self.visible_bindings.iter().map(|(name, index)| {
            let slot = self
                .binding_slots
                .get(*index)
                .expect("visible REPL binding indices are session-owned");
            (name.as_str(), slot)
        })
    }

    #[must_use]
    pub fn type_registry(&self) -> &BTreeMap<String, ReplTypeSlot> {
        &self.type_registry
    }

    #[must_use]
    pub fn resolve_type(&self, name: &str) -> Option<&ReplTypeSlot> {
        self.type_registry.get(name)
    }

    #[must_use]
    pub fn environment(&self) -> &ReplEnvironmentLayout {
        &self.environment
    }

    /// The only entrypoint that a REPL executor should run for this snapshot.
    #[must_use]
    pub fn latest_entry(&self) -> Option<StableSymbolId> {
        self.latest_entry
    }

    #[must_use]
    pub fn candidate_ir(&self) -> Option<&Arc<TypedPackageIr>> {
        self.candidate_ir.as_ref()
    }

    /// Canonical, collision-resistant input for the build fingerprint of this exact session.
    ///
    /// Every variable-width field is length-prefixed, including the package marker on each source
    /// identity, so no two distinct cell histories have the same byte representation merely
    /// because their current revision numbers match.
    #[must_use]
    pub fn canonical_build_context(&self) -> Vec<u8> {
        const DOMAIN: &[u8] = b"nexa.repl.build-context";
        const FORMAT_VERSION: u16 = 1;

        let mut context = Vec::new();
        append_repl_context_bytes(&mut context, DOMAIN);
        context.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        append_repl_context_bytes(&mut context, self.package_id.as_str().as_bytes());
        append_repl_context_bytes(&mut context, self.module.as_str().as_bytes());
        context.extend_from_slice(&self.revision.to_le_bytes());
        context.extend_from_slice(
            &u64::try_from(self.committed_cells.len())
                .expect("REPL cell count fits in the canonical u64 format")
                .to_le_bytes(),
        );
        for cell in self.committed_cells.iter() {
            context.extend_from_slice(&cell.input.ordinal.to_le_bytes());
            match cell.input.source.package_id() {
                Some(package_id) => {
                    context.push(1);
                    append_repl_context_bytes(&mut context, package_id.as_bytes());
                }
                None => context.push(0),
            }
            append_repl_context_bytes(&mut context, cell.input.source.path().as_bytes());
            append_repl_context_bytes(&mut context, cell.input.text.as_bytes());
        }
        context
    }
}

fn append_repl_context_bytes(context: &mut Vec<u8>, value: &[u8]) {
    context.extend_from_slice(
        &u64::try_from(value.len())
            .expect("REPL context field length fits in the canonical u64 format")
            .to_le_bytes(),
    );
    context.extend_from_slice(value);
}

/// A successful cell transaction that has not yet changed session state.
#[derive(Clone, Debug)]
pub struct ReplStagedCell {
    base_revision: u64,
    base_latest_entry: Option<StableSymbolId>,
    input: ReplCellInput,
    bindings: Arc<[ReplBindingSlot]>,
    types: Arc<[ReplTypeSlot]>,
    entry_symbol: StableSymbolId,
    entry_function: DefinitionId,
    entry_span: SourceRange,
    entry_effect: IrEffect,
    result_type: IrType,
    candidate_ir: Option<Arc<TypedPackageIr>>,
}

impl ReplStagedCell {
    #[must_use]
    pub fn input(&self) -> &ReplCellInput {
        &self.input
    }

    #[must_use]
    pub fn bindings(&self) -> &[ReplBindingSlot] {
        &self.bindings
    }

    #[must_use]
    pub fn types(&self) -> &[ReplTypeSlot] {
        &self.types
    }

    #[must_use]
    pub fn entry_symbol(&self) -> StableSymbolId {
        self.entry_symbol
    }

    #[must_use]
    pub fn entry_function(&self) -> DefinitionId {
        self.entry_function
    }

    #[must_use]
    pub fn entry_span(&self) -> &SourceRange {
        &self.entry_span
    }

    #[must_use]
    pub fn entry_effect(&self) -> IrEffect {
        self.entry_effect
    }

    #[must_use]
    pub fn result_type(&self) -> &IrType {
        &self.result_type
    }

    #[must_use]
    pub fn candidate_ir(&self) -> Option<&Arc<TypedPackageIr>> {
        self.candidate_ir.as_ref()
    }

    pub(crate) fn attach_candidate_ir(&mut self, candidate_ir: Arc<TypedPackageIr>) {
        self.candidate_ir = Some(candidate_ir);
    }
}

/// A failed analysis result. It deliberately has no commit token.
#[derive(Clone, Debug)]
pub struct ReplRejectedCell {
    pub input: ReplCellInput,
    pub diagnostics: DiagnosticBatch,
}

/// Outcome of staging one cell. Only `Ready` can be passed to
/// [`ReplAnalysisSession::commit`].
#[derive(Clone, Debug)]
pub enum ReplStagingOutcome {
    Ready(ReplStagedCell),
    Rejected(ReplRejectedCell),
}

impl ReplStagingOutcome {
    #[must_use]
    pub fn ready(self) -> Option<ReplStagedCell> {
        match self {
            Self::Ready(staged) => Some(staged),
            Self::Rejected(_) => None,
        }
    }
}

/// Result of explicitly committing a previously staged cell.
#[derive(Clone, Debug)]
pub struct ReplCommitOutcome {
    pub committed: ReplCommittedCell,
    pub snapshot: ReplSessionSnapshot,
}

/// Transactional state for a cumulative REPL.
#[derive(Clone, Debug)]
pub struct ReplAnalysisSession {
    snapshot: ReplSessionSnapshot,
}

impl Default for ReplAnalysisSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplAnalysisSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshot: ReplSessionSnapshot {
                package_id: repl_package_id(),
                module: repl_module_path(),
                revision: 0,
                committed_cells: Arc::from([]),
                binding_slots: Arc::from([]),
                visible_bindings: BTreeMap::new(),
                type_registry: BTreeMap::new(),
                environment: ReplEnvironmentLayout {
                    class_symbol: repl_environment_symbol(),
                    slots: Arc::from([]),
                },
                latest_entry: None,
                candidate_ir: Some(repl_seed_typed_ir()),
            },
        }
    }

    #[must_use]
    pub fn from_snapshot(snapshot: ReplSessionSnapshot) -> Self {
        Self { snapshot }
    }

    #[must_use]
    pub fn snapshot(&self) -> ReplSessionSnapshot {
        self.snapshot.clone()
    }

    /// Validates an analysis result against the current committed snapshot without mutating it.
    #[allow(clippy::result_large_err, clippy::too_many_lines)]
    pub fn stage_cell(
        &self,
        input: ReplCellInput,
        outcome: ReplCellAnalysisOutcome,
    ) -> Result<ReplStagingOutcome, ReplSessionError> {
        self.validate_input(&input)?;

        let delta = match outcome {
            ReplCellAnalysisOutcome::Accepted(delta) => delta,
            ReplCellAnalysisOutcome::Rejected(diagnostics) => {
                return Ok(ReplStagingOutcome::Rejected(ReplRejectedCell {
                    input,
                    diagnostics,
                }));
            }
        };
        if delta.entry.cell_ordinal != input.ordinal {
            return Err(ReplSessionError::EntrypointOrdinalMismatch {
                cell_ordinal: input.ordinal,
                entry_ordinal: delta.entry.cell_ordinal,
            });
        }
        let expected_entry = repl_cell_entry_symbol(input.ordinal);
        if delta.entry.stable_id != expected_entry {
            return Err(ReplSessionError::EntrypointIdentityMismatch {
                ordinal: input.ordinal,
                expected: expected_entry,
                actual: delta.entry.stable_id,
            });
        }

        let mut staged_bindings = Vec::with_capacity(delta.bindings.len());
        for (index, declaration) in delta.bindings.into_iter().enumerate() {
            if declaration.name.is_empty() {
                return Err(ReplSessionError::EmptyBindingName {
                    ordinal: input.ordinal,
                    declaration_index: index,
                });
            }
            if declaration.mutable && !matches!(&declaration.kind, ReplBindingKind::Value) {
                return Err(ReplSessionError::MutableFunction {
                    ordinal: input.ordinal,
                    name: declaration.name,
                });
            }

            let stable_symbol = declaration.stable_symbol.unwrap_or_else(|| {
                repl_binding_symbol(input.ordinal, index, &declaration.name, &declaration.kind)
            });
            let state_slot =
                matches!(&declaration.kind, ReplBindingKind::Value).then_some(stable_symbol.0);
            staged_bindings.push(ReplBindingSlot {
                name: declaration.name,
                definition: declaration.definition,
                stable_symbol,
                state_slot,
                kind: declaration.kind,
                mutable: declaration.mutable,
                ty: declaration.ty,
                defining_cell: input.ordinal,
                span: declaration.span,
            });
        }

        let mut cell_type_names = BTreeSet::new();
        let mut staged_types = Vec::with_capacity(delta.types.len());
        for (index, declaration) in delta.types.into_iter().enumerate() {
            if declaration.name.is_empty() {
                return Err(ReplSessionError::EmptyTypeName {
                    ordinal: input.ordinal,
                    declaration_index: index,
                });
            }
            if let Some(prior) = self.snapshot.type_registry.get(&declaration.name) {
                return Err(ReplSessionError::TypeAlreadyDefined {
                    name: declaration.name,
                    defining_cell: prior.defining_cell,
                    attempted_cell: input.ordinal,
                    original: prior.span.clone(),
                    attempted: declaration.span,
                });
            }
            if !cell_type_names.insert(declaration.name.clone()) {
                return Err(ReplSessionError::TypeDefinedTwiceInCell {
                    name: declaration.name,
                    ordinal: input.ordinal,
                });
            }

            staged_types.push(ReplTypeSlot {
                stable_symbol: repl_type_symbol(&declaration.name),
                name: declaration.name,
                definition: declaration.definition,
                ty: declaration.ty,
                defining_cell: input.ordinal,
                span: declaration.span,
            });
        }

        Ok(ReplStagingOutcome::Ready(ReplStagedCell {
            base_revision: self.snapshot.revision,
            base_latest_entry: self.snapshot.latest_entry,
            entry_symbol: delta.entry.stable_id,
            entry_function: delta.entry.function,
            entry_span: delta.entry_span,
            input,
            bindings: staged_bindings.into(),
            types: staged_types.into(),
            entry_effect: delta.entry.effect,
            result_type: delta.entry.result,
            candidate_ir: None,
        }))
    }

    /// Atomically installs one successful staged delta.
    ///
    /// A stale stage is rejected before any field changes, so callers can safely re-analyze
    /// against the returned current snapshot.
    #[allow(clippy::result_large_err)]
    pub fn commit(
        &mut self,
        staged: ReplStagedCell,
    ) -> Result<ReplCommitOutcome, ReplSessionError> {
        if staged.base_revision != self.snapshot.revision
            || staged.base_latest_entry != self.snapshot.latest_entry
        {
            return Err(ReplSessionError::StaleStage {
                staged_revision: staged.base_revision,
                current_revision: self.snapshot.revision,
            });
        }
        self.validate_input(&staged.input)?;
        let next_revision = self
            .snapshot
            .revision
            .checked_add(1)
            .ok_or(ReplSessionError::RevisionExhausted)?;

        let mut binding_slots = self.snapshot.binding_slots.to_vec();
        let mut visible_bindings = self.snapshot.visible_bindings.clone();
        let mut state_slots = self.snapshot.environment.slots.to_vec();
        let mut binding_symbols = Vec::with_capacity(staged.bindings.len());
        for binding in staged.bindings.iter().cloned() {
            let index = binding_slots.len();
            binding_symbols.push(binding.stable_symbol);
            visible_bindings.insert(binding.name.clone(), index);
            if let Some(stable_id) = binding.state_slot {
                state_slots.push(ReplStateSlot {
                    name: binding.name.clone(),
                    definition: binding.definition,
                    stable_id,
                    binding_symbol: binding.stable_symbol,
                    mutable: binding.mutable,
                    ty: binding.ty.clone(),
                    defining_cell: binding.defining_cell,
                    span: binding.span.clone(),
                });
            }
            binding_slots.push(binding);
        }

        let mut type_registry = self.snapshot.type_registry.clone();
        let mut type_symbols = Vec::with_capacity(staged.types.len());
        for type_slot in staged.types.iter().cloned() {
            if let Some(prior) = type_registry.get(&type_slot.name) {
                return Err(ReplSessionError::TypeAlreadyDefined {
                    name: type_slot.name,
                    defining_cell: prior.defining_cell,
                    attempted_cell: staged.input.ordinal,
                    original: prior.span.clone(),
                    attempted: type_slot.span,
                });
            }
            type_symbols.push(type_slot.stable_symbol);
            type_registry.insert(type_slot.name.clone(), type_slot);
        }

        let committed = ReplCommittedCell {
            input: staged.input,
            entry_symbol: staged.entry_symbol,
            entry_function: staged.entry_function,
            entry_span: staged.entry_span,
            entry_effect: staged.entry_effect,
            result_type: staged.result_type,
            binding_symbols: binding_symbols.into(),
            type_symbols: type_symbols.into(),
        };
        let mut committed_cells = self.snapshot.committed_cells.to_vec();
        committed_cells.push(committed.clone());

        let next_snapshot = ReplSessionSnapshot {
            package_id: self.snapshot.package_id.clone(),
            module: self.snapshot.module.clone(),
            revision: next_revision,
            committed_cells: committed_cells.into(),
            binding_slots: binding_slots.into(),
            visible_bindings,
            type_registry,
            environment: ReplEnvironmentLayout {
                class_symbol: self.snapshot.environment.class_symbol,
                slots: state_slots.into(),
            },
            latest_entry: Some(committed.entry_symbol),
            candidate_ir: staged
                .candidate_ir
                .or_else(|| self.snapshot.candidate_ir.clone()),
        };
        self.snapshot = next_snapshot.clone();

        Ok(ReplCommitOutcome {
            committed,
            snapshot: next_snapshot,
        })
    }

    #[allow(clippy::result_large_err)]
    fn validate_input(&self, input: &ReplCellInput) -> Result<(), ReplSessionError> {
        if input.ordinal < REPL_FIRST_CELL_ORDINAL {
            return Err(ReplSessionError::InvalidOrdinal {
                offered: input.ordinal,
            });
        }
        if let Some(last) = self.snapshot.last_committed_ordinal()
            && input.ordinal <= last
        {
            return Err(ReplSessionError::OrdinalNotIncreasing {
                last_committed: last,
                offered: input.ordinal,
            });
        }
        if input.source.package_id() != Some(REPL_PACKAGE_ID) {
            return Err(ReplSessionError::InvalidSourcePackage {
                expected: REPL_PACKAGE_ID,
                actual: input.source.package_id().map(str::to_owned),
            });
        }
        if input.source.path().is_empty() {
            return Err(ReplSessionError::EmptySourcePath {
                ordinal: input.ordinal,
            });
        }
        Ok(())
    }
}

#[must_use]
pub fn repl_package_id() -> PackageId {
    PackageId::new(REPL_PACKAGE_ID).expect("the static REPL package ID is valid")
}

#[must_use]
pub fn repl_module_path() -> ModulePath {
    ModulePath::new(REPL_MODULE_PATH).expect("the static REPL module path is valid")
}

/// Stable identity of the synthetic entrypoint for a cell.
#[must_use]
pub fn repl_cell_entry_symbol(ordinal: u64) -> StableSymbolId {
    repl_symbol(SymbolKind::Function, &format!("cell_{ordinal}"))
}

/// Stable identity of one binding occurrence. The declaration index keeps same-cell shadowing
/// unambiguous without rewriting source.
#[must_use]
pub fn repl_binding_symbol(
    ordinal: u64,
    declaration_index: usize,
    name: &str,
    kind: &ReplBindingKind,
) -> StableSymbolId {
    repl_binding_identity(ordinal, declaration_index, name, kind).runtime_id()
}

pub(crate) fn repl_binding_identity(
    ordinal: u64,
    declaration_index: usize,
    name: &str,
    kind: &ReplBindingKind,
) -> CanonicalSymbolIdentity {
    let namespace = match kind {
        ReplBindingKind::Value => "value",
        ReplBindingKind::Function { .. } => "function",
    };
    let kind = match kind {
        ReplBindingKind::Value => SymbolKind::Field,
        ReplBindingKind::Function { .. } => SymbolKind::Function,
    };
    // The reserved occurrence name is the analyzer-owned persistent identity. The source-visible
    // name remains available to diagnostics and `:bytecode`, but is deliberately not reconstructed
    // as an automatic module symbol: shadowed REPL declarations are distinct occurrences.
    CanonicalSymbolIdentity::explicit(
        REPL_PACKAGE_ID,
        kind,
        format!("__repl_cell_{ordinal}_{namespace}_{declaration_index}_{name}"),
    )
}

/// Stable identity of a nominal REPL type. Type names cannot be redefined, so the cell ordinal is
/// intentionally absent.
#[must_use]
pub fn repl_type_symbol(name: &str) -> StableSymbolId {
    repl_symbol(SymbolKind::Type, name)
}

#[must_use]
pub fn repl_environment_symbol() -> StableSymbolId {
    repl_symbol(SymbolKind::Type, REPL_ENVIRONMENT_TYPE_NAME)
}

/// Formal revision-zero candidate for a cumulative REPL session.
///
/// The seed reserves the hidden environment state ABI before any cell is compiled. It has no
/// executable entrypoint or lifecycle function. Runtime session setup must create the unique
/// empty environment instance identified by [`repl_environment_symbol`] before staging cell 1.
#[must_use]
pub fn repl_seed_typed_ir() -> Arc<TypedPackageIr> {
    let package_id = repl_package_id();
    let module = repl_module_path();
    let source = SourceKey::new(
        package_id.clone(),
        // The seed owns artifact FileId 1 for the lifetime of the session. Keep its canonical
        // source identity before every `cell_N.nexa` identity so the cumulative compiler source
        // table remains sorted by the same authority used by package artifact validation.
        NormalizedPackagePath::new("src/__repl/_environment.nexa")
            .expect("the static REPL seed path is normalized"),
    );
    let text = format!(
        "@state(version = {REPL_ENVIRONMENT_STATE_VERSION})\nclass \
         {REPL_ENVIRONMENT_TYPE_NAME} {{\n}}\n"
    );
    let syntax = Arc::new(
        nexa_syntax::parse_nexa(&text).expect("the static REPL seed source fits syntax limits"),
    );
    let end = u32::try_from(text.len()).expect("the static REPL seed source fits u32");
    let span = SourceRange {
        source: source.clone(),
        start: 0,
        end,
    };
    let definition_id = DefinitionId(0);
    let canonical = CanonicalSymbolIdentity::automatic(
        REPL_PACKAGE_ID,
        REPL_MODULE_PATH,
        SymbolKind::Type,
        REPL_ENVIRONMENT_TYPE_NAME,
    );
    let stable_id = canonical.runtime_id();
    let definitions = vec![Definition {
        id: definition_id,
        package_id: package_id.clone(),
        module: module.clone(),
        name: REPL_ENVIRONMENT_TYPE_NAME.to_owned(),
        kind: DefinitionKind::Class,
        visibility: DeclarationVisibility::Private,
        ty: IrType::Named(definition_id),
        effect: IrEffect::Immediate,
        span,
        canonical_identity: format!(
            "{REPL_PACKAGE_ID}::{REPL_MODULE_PATH}::{:?}::{REPL_ENVIRONMENT_TYPE_NAME}",
            SymbolKind::Type
        ),
        stable_symbol: Some(StableSymbolIdentity {
            canonical,
            runtime_id: stable_id,
        }),
    }];
    let state_types = vec![StateTypeIr {
        definition: definition_id,
        version: REPL_ENVIRONMENT_STATE_VERSION,
        stable_id,
        fields: Vec::new(),
    }];
    let state_schema_fingerprint = canonical_state_schema(&state_types, &definitions)
        .expect("the zero-field REPL environment is a valid state schema")
        .fingerprint();
    let metadata = PackageSemanticMetadata {
        entry_module: Some(module.clone()),
        state_types: state_types.into(),
        host_bindings: Arc::from([]),
        exports: Arc::from([]),
        tests: Arc::from([]),
        external_sources: Arc::from([]),
        lifecycle: LifecycleBindingsIr::default(),
        repl_entry: None,
        standard_functions: Arc::from([]),
        public_api_fingerprint: public_api_fingerprint([]),
        state_schema_fingerprint,
    };
    let modules = vec![TypedModuleIr {
        package_id: package_id.clone(),
        module: module.clone(),
        virtual_module_path: Some(module),
        source,
        file_id: ArtifactFileId(1),
        syntax,
        resolved_references: Arc::from([]),
        declarations: vec![TypedDeclarationIr {
            definition: definition_id,
            body: TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Class {
                fields: Vec::new(),
                state: Some(StateMetadataIr {
                    version: REPL_ENVIRONMENT_STATE_VERSION,
                    stable_id,
                }),
            }),
        }]
        .into(),
    }];
    Arc::new(
        TypedPackageIr::new_repl_cell(package_id, 0, definitions, modules, metadata)
            .expect("the formal REPL seed Typed IR is valid"),
    )
}

fn repl_symbol(kind: SymbolKind, name: &str) -> StableSymbolId {
    CanonicalSymbolIdentity::automatic(REPL_PACKAGE_ID, REPL_MODULE_PATH, kind, name).runtime_id()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplSessionError {
    InvalidOrdinal {
        offered: u64,
    },
    OrdinalNotIncreasing {
        last_committed: u64,
        offered: u64,
    },
    InvalidSourcePackage {
        expected: &'static str,
        actual: Option<String>,
    },
    EmptySourcePath {
        ordinal: u64,
    },
    EmptyBindingName {
        ordinal: u64,
        declaration_index: usize,
    },
    MutableFunction {
        ordinal: u64,
        name: String,
    },
    EmptyTypeName {
        ordinal: u64,
        declaration_index: usize,
    },
    TypeAlreadyDefined {
        name: String,
        defining_cell: u64,
        attempted_cell: u64,
        original: SourceRange,
        attempted: SourceRange,
    },
    TypeDefinedTwiceInCell {
        name: String,
        ordinal: u64,
    },
    StaleStage {
        staged_revision: u64,
        current_revision: u64,
    },
    EntrypointOrdinalMismatch {
        cell_ordinal: u64,
        entry_ordinal: u64,
    },
    EntrypointIdentityMismatch {
        ordinal: u64,
        expected: StableSymbolId,
        actual: StableSymbolId,
    },
    RevisionExhausted,
}

impl fmt::Display for ReplSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOrdinal { offered } => {
                write!(
                    formatter,
                    "REPL cell ordinal {offered} is invalid; ordinals start at one"
                )
            }
            Self::OrdinalNotIncreasing {
                last_committed,
                offered,
            } => write!(
                formatter,
                "REPL cell ordinal {offered} must be greater than committed ordinal \
                 {last_committed}"
            ),
            Self::InvalidSourcePackage { expected, actual } => write!(
                formatter,
                "REPL source package must be `{expected}`, found {}",
                actual.as_deref().unwrap_or("<standalone>")
            ),
            Self::EmptySourcePath { ordinal } => {
                write!(formatter, "REPL cell {ordinal} has an empty source path")
            }
            Self::EmptyBindingName {
                ordinal,
                declaration_index,
            } => write!(
                formatter,
                "REPL cell {ordinal} binding declaration {declaration_index} has an empty name"
            ),
            Self::MutableFunction { ordinal, name } => write!(
                formatter,
                "REPL cell {ordinal} function `{name}` cannot be declared mutable"
            ),
            Self::EmptyTypeName {
                ordinal,
                declaration_index,
            } => write!(
                formatter,
                "REPL cell {ordinal} type declaration {declaration_index} has an empty name"
            ),
            Self::TypeAlreadyDefined {
                name,
                defining_cell,
                attempted_cell,
                ..
            } => write!(
                formatter,
                "REPL cell {attempted_cell} cannot redefine type `{name}` from cell \
                 {defining_cell}"
            ),
            Self::TypeDefinedTwiceInCell { name, ordinal } => write!(
                formatter,
                "REPL cell {ordinal} defines type `{name}` more than once"
            ),
            Self::StaleStage {
                staged_revision,
                current_revision,
            } => write!(
                formatter,
                "REPL stage was based on revision {staged_revision}, but the session is at \
                revision {current_revision}"
            ),
            Self::EntrypointOrdinalMismatch {
                cell_ordinal,
                entry_ordinal,
            } => write!(
                formatter,
                "REPL cell {cell_ordinal} produced entrypoint ordinal {entry_ordinal}"
            ),
            Self::EntrypointIdentityMismatch {
                ordinal,
                expected,
                actual,
            } => write!(
                formatter,
                "REPL cell {ordinal} entrypoint identity must be {expected}, found {actual}"
            ),
            Self::RevisionExhausted => formatter.write_str("REPL session revision is exhausted"),
        }
    }
}

impl std::error::Error for ReplSessionError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexa_diagnostics::{DiagnosticBatch, SourceIdentity, SourceSnapshotRegistry};

    use super::{
        REPL_PACKAGE_ID, ReplAnalysisSession, ReplBindingDeclaration, ReplBindingKind,
        ReplCellAnalysisOutcome, ReplCellDelta, ReplCellInput, ReplSessionError,
        ReplStagingOutcome, ReplTypeDeclaration, repl_cell_entry_symbol,
    };
    use crate::{
        DefinitionId, IrEffect, IrType, NormalizedPackagePath, PackageId, ReplEntrypointIr,
        SourceKey, SourceRange,
    };

    fn source(ordinal: u64) -> SourceKey {
        SourceKey::new(
            PackageId::new(REPL_PACKAGE_ID).unwrap(),
            NormalizedPackagePath::new(format!("repl/session/cell-{ordinal}.nexa")).unwrap(),
        )
    }

    fn input(ordinal: u64, text: &str) -> ReplCellInput {
        ReplCellInput::new(
            ordinal,
            SourceIdentity::package(REPL_PACKAGE_ID, format!("repl/session/cell-{ordinal}.nexa")),
            text,
        )
    }

    fn span(ordinal: u64) -> SourceRange {
        SourceRange {
            source: source(ordinal),
            start: 0,
            end: 1,
        }
    }

    fn delta(
        ordinal: u64,
        bindings: Vec<ReplBindingDeclaration>,
        types: Vec<ReplTypeDeclaration>,
    ) -> ReplCellDelta {
        ReplCellDelta::new(
            bindings,
            types,
            ReplEntrypointIr {
                cell_ordinal: ordinal,
                function: DefinitionId(10_000 + u32::try_from(ordinal).unwrap()),
                stable_id: repl_cell_entry_symbol(ordinal),
                result: IrType::Unit,
                effect: IrEffect::Ordinary,
            },
            span(ordinal),
        )
    }

    fn ready(outcome: ReplStagingOutcome) -> super::ReplStagedCell {
        match outcome {
            ReplStagingOutcome::Ready(staged) => staged,
            ReplStagingOutcome::Rejected(_) => panic!("expected a successful stage"),
        }
    }

    fn assert_same_snapshot(
        actual: &super::ReplSessionSnapshot,
        expected: &super::ReplSessionSnapshot,
    ) {
        assert_eq!(actual.package_id(), expected.package_id());
        assert_eq!(actual.module(), expected.module());
        assert_eq!(actual.revision(), expected.revision());
        assert_eq!(actual.committed_cells(), expected.committed_cells());
        assert_eq!(actual.binding_slots(), expected.binding_slots());
        assert_eq!(actual.type_registry(), expected.type_registry());
        assert_eq!(actual.environment(), expected.environment());
        assert_eq!(actual.latest_entry(), expected.latest_entry());
        match (actual.candidate_ir(), expected.candidate_ir()) {
            (None, None) => {}
            (Some(actual), Some(expected)) => assert!(Arc::ptr_eq(actual, expected)),
            _ => panic!("candidate IR presence changed"),
        }
    }

    #[test]
    fn value_and_function_shadowing_preserves_historical_slots() {
        let mut session = ReplAnalysisSession::new();
        let first = ready(
            session
                .stage_cell(
                    input(1, "let mut answer = 41;"),
                    ReplCellAnalysisOutcome::Accepted(delta(
                        1,
                        vec![ReplBindingDeclaration::value(
                            "answer",
                            DefinitionId(1),
                            true,
                            IrType::I32,
                            span(1),
                        )],
                        vec![],
                    )),
                )
                .unwrap(),
        );
        assert!(session.snapshot().latest_entry().is_none());
        let first_commit = session.commit(first).unwrap();
        let first_symbol = first_commit.snapshot.binding_slots()[0].stable_symbol;

        let second = ready(
            session
                .stage_cell(
                    input(2, "fn answer() -> i32 { return 42; }"),
                    ReplCellAnalysisOutcome::Accepted(delta(
                        2,
                        vec![ReplBindingDeclaration::function(
                            "answer",
                            DefinitionId(2),
                            Arc::<[IrType]>::from([]),
                            IrType::I32,
                            IrEffect::Ordinary,
                            span(2),
                        )],
                        vec![],
                    )),
                )
                .unwrap(),
        );
        let second_commit = session.commit(second).unwrap();

        assert_eq!(second_commit.snapshot.binding_slots().len(), 2);
        assert_eq!(second_commit.snapshot.environment().slots.len(), 1);
        assert_eq!(
            second_commit.snapshot.environment().slots[0].binding_symbol,
            first_symbol
        );
        let visible = second_commit
            .snapshot
            .visible_binding("answer")
            .expect("shadow is visible");
        assert!(matches!(&visible.kind, ReplBindingKind::Function { .. }));
        assert_ne!(visible.stable_symbol, first_symbol);
        assert_eq!(
            second_commit.snapshot.latest_entry(),
            Some(second_commit.committed.entry_symbol)
        );
    }

    #[test]
    fn type_redefinition_is_rejected_without_mutating_the_session() {
        let mut session = ReplAnalysisSession::new();
        let first = ready(
            session
                .stage_cell(
                    input(1, "struct Point { x: i32 }"),
                    ReplCellAnalysisOutcome::Accepted(delta(
                        1,
                        vec![],
                        vec![ReplTypeDeclaration::new(
                            "Point",
                            DefinitionId(3),
                            IrType::Unit,
                            span(1),
                        )],
                    )),
                )
                .unwrap(),
        );
        session.commit(first).unwrap();
        let before = session.snapshot();

        let error = session
            .stage_cell(
                input(2, "struct Point { y: i32 }"),
                ReplCellAnalysisOutcome::Accepted(delta(
                    2,
                    vec![],
                    vec![ReplTypeDeclaration::new(
                        "Point",
                        DefinitionId(4),
                        IrType::Unit,
                        span(2),
                    )],
                )),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            ReplSessionError::TypeAlreadyDefined {
                defining_cell: 1,
                attempted_cell: 2,
                ..
            }
        ));
        assert_same_snapshot(&session.snapshot(), &before);
    }

    #[test]
    fn rejected_analysis_has_no_commit_token_and_does_not_advance_state() {
        let mut session = ReplAnalysisSession::new();
        let before = session.snapshot();
        let diagnostics =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));

        let outcome = session
            .stage_cell(
                input(4, "not valid"),
                ReplCellAnalysisOutcome::Rejected(diagnostics),
            )
            .unwrap();

        assert!(matches!(outcome, ReplStagingOutcome::Rejected(_)));
        assert_same_snapshot(&session.snapshot(), &before);

        let next = ready(
            session
                .stage_cell(
                    input(5, "42"),
                    ReplCellAnalysisOutcome::Accepted(delta(5, vec![], vec![])),
                )
                .unwrap(),
        );
        let committed = session.commit(next).unwrap();
        assert_eq!(committed.committed.ordinal(), 5);
    }

    #[test]
    fn canonical_build_context_distinguishes_equal_revisions_with_different_histories() {
        let mut left = ReplAnalysisSession::new();
        let left_cell = ready(
            left.stage_cell(
                input(1, "let answer = 41;"),
                ReplCellAnalysisOutcome::Accepted(delta(1, vec![], vec![])),
            )
            .unwrap(),
        );
        left.commit(left_cell).unwrap();

        let mut right = ReplAnalysisSession::new();
        let right_cell = ready(
            right
                .stage_cell(
                    ReplCellInput::new(
                        1,
                        SourceIdentity::package(
                            REPL_PACKAGE_ID,
                            "repl/session/alternate-cell-1.nexa",
                        ),
                        "let answer = 42;",
                    ),
                    ReplCellAnalysisOutcome::Accepted(delta(1, vec![], vec![])),
                )
                .unwrap(),
        );
        right.commit(right_cell).unwrap();

        assert_eq!(left.snapshot().revision(), right.snapshot().revision());
        assert_ne!(
            left.snapshot().canonical_build_context(),
            right.snapshot().canonical_build_context()
        );
    }

    #[test]
    fn committing_one_branch_makes_another_stage_stale() {
        let mut session = ReplAnalysisSession::new();
        let first = ready(
            session
                .stage_cell(
                    input(1, "1"),
                    ReplCellAnalysisOutcome::Accepted(delta(1, vec![], vec![])),
                )
                .unwrap(),
        );
        let competing = ready(
            session
                .stage_cell(
                    input(2, "2"),
                    ReplCellAnalysisOutcome::Accepted(delta(2, vec![], vec![])),
                )
                .unwrap(),
        );

        session.commit(first).unwrap();
        let before = session.snapshot();
        assert!(matches!(
            session.commit(competing),
            Err(ReplSessionError::StaleStage { .. })
        ));
        assert_same_snapshot(&session.snapshot(), &before);
    }

    #[test]
    fn committed_ordinals_are_strictly_increasing_but_may_skip_failed_cells() {
        let mut session = ReplAnalysisSession::new();
        let staged = ready(
            session
                .stage_cell(
                    input(3, "42"),
                    ReplCellAnalysisOutcome::Accepted(delta(3, vec![], vec![])),
                )
                .unwrap(),
        );
        session.commit(staged).unwrap();

        assert!(matches!(
            session.stage_cell(
                input(3, "43"),
                ReplCellAnalysisOutcome::Accepted(delta(3, vec![], vec![]))
            ),
            Err(ReplSessionError::OrdinalNotIncreasing { .. })
        ));
    }
}
