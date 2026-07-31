use std::collections::BTreeSet;
use std::sync::Arc;

use nexa_analysis::SourceKey;
use nexa_bytecode::{FunctionEffect, Module};
use nexa_core::{
    CanonicalSymbolIdentity, FileId, PublicApiFingerprint, SourceSpan, StableId, StableSymbolId,
    StateSchemaFingerprint, SymbolKind,
};
use nexa_diagnostics::SourceIdentity;
use nexa_stdlib::standard_library;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageCompileOutput {
    pub module: Module,
    pub test_module: Option<Module>,
    /// Exact deterministic source catalog for every `FileId` retained by product codegen.
    pub sources: Vec<PackageCompiledSource>,
    /// Exact deterministic source catalog for every `FileId` retained by test codegen.
    pub test_sources: Vec<PackageCompiledSource>,
    pub debug_info: PackageDebugInfo,
    pub test_debug_info: Option<PackageDebugInfo>,
    pub public_symbols: Vec<PackagePublicSymbol>,
    pub state_surface: Vec<PackageStateTypeInfo>,
    pub tests: Vec<PackageTestInfo>,
    pub test_call_graph: Vec<PackageTestCallGraphNode>,
    pub standard_library: PackageStandardLibraryInfo,
    pub public_api_fingerprint: Option<PublicApiFingerprint>,
    pub state_schema_fingerprint: Option<StateSchemaFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageCompiledSource {
    pub source_key: Option<SourceKey>,
    pub identity: SourceIdentity,
    pub package_id: Option<String>,
    pub module_path: Option<String>,
    pub path: String,
    pub file: FileId,
    pub source: Arc<str>,
    pub compiler_provided: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageDebugInfo {
    pub root_package_id: String,
    pub entry_module: String,
    pub modules: Vec<PackageModuleDebugInfo>,
    pub functions: Vec<PackageFunctionDebugInfo>,
    pub host_imports: Vec<PackageHostImportDebugInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageModuleDebugInfo {
    pub package_id: String,
    pub module_path: String,
    pub file: FileId,
    pub definition_span: SourceSpan,
    pub source_span: SourceSpan,
    pub function_indices: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageFunctionDebugInfo {
    pub function_index: u32,
    pub package_id: String,
    pub module_path: String,
    pub name: String,
    pub canonical_identity: CanonicalSymbolIdentity,
    pub stable_id: StableSymbolId,
    pub definition_span: SourceSpan,
    pub effect: FunctionEffect,
    pub visibility: PackageVisibility,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageHostImportDebugInfo {
    pub import_index: u32,
    pub stable_id: StableId,
    pub interface_id: StableId,
    pub interface_name: String,
    pub function_name: String,
    pub interface_span: SourceSpan,
    pub declaration_span: SourceSpan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PackageVisibility {
    Private,
    Package,
    Public,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagePublicSymbol {
    pub package_id: String,
    pub module_path: String,
    pub name: String,
    pub kind: SymbolKind,
    pub canonical_identity: CanonicalSymbolIdentity,
    pub stable_id: StableSymbolId,
    pub definition_span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageStateTypeInfo {
    pub package_id: String,
    pub module_path: String,
    pub name: String,
    pub version: u32,
    pub canonical_identity: CanonicalSymbolIdentity,
    pub stable_id: StableSymbolId,
    pub definition_span: SourceSpan,
    pub fields: Vec<PackageStateFieldInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageStateFieldInfo {
    pub name: String,
    pub canonical_identity: CanonicalSymbolIdentity,
    pub stable_id: StableSymbolId,
    pub definition_span: SourceSpan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageTestRejection {
    ParametersMustBeEmpty,
    ResultMustBeBool,
    EffectMustBeImmediate,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageTestInfo {
    pub package_id: String,
    pub module_path: String,
    pub name: String,
    pub function_index: u32,
    pub canonical_identity: CanonicalSymbolIdentity,
    pub stable_id: StableSymbolId,
    pub definition_span: SourceSpan,
    pub effect: FunctionEffect,
    pub rejection: Option<PackageTestRejection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PackageTestForbiddenEffect {
    Host,
    Task,
    Await,
    Yield,
    Activation,
    Migration,
    PersistentState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageTestCallGraphNode {
    pub function_index: u32,
    pub calls: Vec<u32>,
    pub forbidden_effects: BTreeSet<PackageTestForbiddenEffect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageStandardLibraryInfo {
    pub package_id: String,
    pub canonical_package_id: String,
    pub version: String,
    pub descriptor_schema: u16,
    pub descriptor_hash: u64,
}

pub(crate) fn standard_library_info() -> PackageStandardLibraryInfo {
    let library = standard_library();
    PackageStandardLibraryInfo {
        package_id: library.package_id.to_owned(),
        canonical_package_id: library.canonical_package_id.to_owned(),
        version: library.version.to_string(),
        descriptor_schema: library.descriptor_schema,
        descriptor_hash: library.descriptor_hash().0,
    }
}
