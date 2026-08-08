//! Package-scale source, manifest, dependency, fingerprint, and incremental-analysis models.
//!
//! This crate intentionally contains no bytecode or runtime types. It describes the immutable
//! input to a package analysis and exposes deterministic identities that the compiler, CLI, LSP,
//! and embedding facade can share.

mod analyzer;
mod candidate;
mod contract_scan;
mod development;
mod entrypoints;
mod fingerprint;
mod graph;
mod identity;
mod ir;
mod loader;
mod lockfile;
mod manifest;
mod options;
pub mod passes;
mod query;
mod repl;
mod semantic;
mod snippet;
mod source;

pub use analyzer::*;
pub use candidate::{
    CandidateError, CandidateIdentity, FreshnessMismatch, FreshnessOutcome, PackageCandidate,
    ResolvedBuildInput, ResolvedBuildInputError, ResolvedTestInput,
};
pub use contract_scan::{
    EffectiveContractReferences, EffectiveContractScanError, effective_contract_references,
};
pub use development::*;
pub use entrypoints::{
    EffectiveEntrypointScanError, EffectiveEntrypointSet, effective_entrypoint_set,
};
pub use fingerprint::{
    BuildFingerprint, BuildFingerprintInput, FingerprintBuilder, LinkedStateFingerprint,
    PublicApiFingerprint, SemanticFingerprintRecord, SourceSetFingerprint, StateSchemaFingerprint,
    canonical_state_schema, canonical_value_type, public_api_fingerprint, source_set_fingerprint,
    state_schema_fingerprint,
};
pub use graph::{
    DependencyEdge, DependencyGraphError, DependencyIdentityConflict, GraphCycle, ModuleGraph,
    ModuleGraphError, PackageCatalog, PackageLocation, ResolvedDependencyGraph, ResolvedPackage,
};
pub use identity::{
    ArtifactFileId, DependencyAlias, DependencyPath, IdentityError, ModulePath,
    NormalizedPackagePath, PackageId, SourceId, SourceKey, external_source_key,
};
pub use ir::{
    BinaryOperator, BuiltinOperationIr, BuiltinVariantIr, CollectionIterationKindIr, Definition,
    DefinitionId, DefinitionKind, ExportBindingIr, ExternalSourceRangeIr, ExternalSourceSnapshotIr,
    FieldLayoutIr, HostAsyncResultIr, HostBindingIr, HostFieldBindingIr, HostFunctionBindingIr,
    HostNamespaceBindingIr, HostTypeBindingIr, HostTypeLayoutIr, HostVariantBindingIr,
    IrAbandonPolicy, IrCancelPolicy, IrCompilationKind, IrEffect, IrHostFunctionMode, IrLiteral,
    IrType, LifecycleBindingsIr, MigrationIntrinsicIr, PackageSemanticMetadata, ReplEntrypointIr,
    ResolvedReference, SourceRange, StableSymbolIdentity, StandardFunctionBindingIr, StateFieldIr,
    StateMetadataIr, StateTypeIr, TestDefinitionIr, TypedBlockIr, TypedDeclarationBody,
    TypedDeclarationIr, TypedExpressionIr, TypedExpressionKind, TypedFunctionIr, TypedIrError,
    TypedMatchArmIr, TypedModuleIr, TypedPackageIr, TypedPatternIr, TypedPatternKind, TypedPlaceIr,
    TypedStatementIr, TypedTypeLayoutIr, UnaryOperator, VariantLayoutIr,
};
pub use loader::{
    LoadedPackageDirectory, PackageLoadError, load_package_directory,
    load_package_directory_without_lock, validate_module_source, validate_module_source_for_role,
};
pub use lockfile::{LockDrift, LockError, LockFile, LockedDependencyEdge, LockedPackage};
pub use manifest::{
    ActivationPolicy, ApplicationSettings, ManifestError, PackageKind, PackageManifest,
    PathDependency,
};
pub use options::{
    COMPILATION_OPTIONS_SCHEMA_VERSION, CompilationOptions, CompilationProfile,
    DEFAULT_MAX_WHILE_ITERATIONS, NEXA_LANGUAGE_VERSION, canonical_compilation_options,
};
pub use query::{
    BuildInputUpdate, CachedTypedModule, ChangeImpact, DeclarationHeader, DeclarationVisibility,
    HeaderDeclarationKind, HeaderError, ImportHeader, InvalidationReport, ModuleHeader, ModuleKey,
    QueryDatabase, QueryExecutionReport, QueryKey, QueryStats, QueryValue, SourceSetChange,
    SourceUpdate, SourceUpdateError,
};
pub use repl::*;
pub use semantic::{
    InstantiatedParameter, InstantiatedSignature, call_signature_at, definition_at, display_type,
    semantic_span_at, type_at,
};
pub use snippet::{
    DEFAULT_SNIPPET_MODULE, SnippetModuleInferenceError, SnippetModuleInferenceErrorKind,
    infer_snippet_module,
};
pub use source::{
    ArtifactFile, ArtifactFileTable, CompilationLimits, PackageSourceSet, SourceDiscoveryError,
    SourceRole, SourceSetBuilder, SourceSetError, SourceUnit,
};
