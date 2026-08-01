use std::collections::BTreeMap;
use std::sync::Arc;

use nexa_bytecode::{FunctionEffect, Signature};
use nexa_core::{FileId, SourceSpan, StableId};

use crate::CompiledPackageArtifact;

/// Reader-facing visibility of one compiled Package function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageSymbolVisibility {
    Private,
    Package,
    Public,
}

impl From<nexa_compiler::PackageVisibility> for PackageSymbolVisibility {
    fn from(value: nexa_compiler::PackageVisibility) -> Self {
        match value {
            nexa_compiler::PackageVisibility::Private => Self::Private,
            nexa_compiler::PackageVisibility::Package => Self::Package,
            nexa_compiler::PackageVisibility::Public => Self::Public,
        }
    }
}

/// Stable, source-addressable inspection record for one compiled function.
///
/// Dense bytecode function slots are deliberately absent. Callers which intentionally need the
/// low-level bytecode representation can inspect [`CompiledPackageArtifact::module`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageFunctionInspection {
    pub stable_id: StableId,
    pub package_id: String,
    pub module_path: String,
    pub name: String,
    pub definition_span: SourceSpan,
    pub signature: Option<Signature>,
    pub effect: FunctionEffect,
    pub visibility: PackageSymbolVisibility,
}

/// Stable, source-addressable inspection record for one compiled module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageModuleInspection {
    pub package_id: String,
    pub module_path: String,
    pub file: FileId,
    pub definition_span: SourceSpan,
    pub source_span: SourceSpan,
    pub function_stable_ids: Arc<[StableId]>,
}

/// Stable Host declaration provenance retained by a compiled Package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageHostImportInspection {
    pub stable_id: StableId,
    pub contract_id: StableId,
    pub contract_name: String,
    pub function_name: String,
    pub contract_span: SourceSpan,
    pub declaration_span: SourceSpan,
}

/// High-level debug/source inspection surface for a verified Package artifact.
///
/// This is intentionally separate from compiler-private dense slot metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageDebugInspection {
    pub root_package_id: String,
    pub entry_module: String,
    pub modules: Arc<[PackageModuleInspection]>,
    pub functions: Arc<[PackageFunctionInspection]>,
    pub host_imports: Arc<[PackageHostImportInspection]>,
}

impl CompiledPackageArtifact {
    /// Returns StableId/source-based inspection metadata without exposing bytecode function slots.
    #[must_use]
    pub fn debug_inspection(&self) -> PackageDebugInspection {
        let functions = self
            .debug_info
            .functions
            .iter()
            .map(|function| package_function_inspection(self, function))
            .collect::<Vec<_>>();
        let stable_by_slot = self
            .debug_info
            .functions
            .iter()
            .map(|function| (function.function_index, function.stable_id.0))
            .collect::<BTreeMap<_, _>>();
        let modules = self
            .debug_info
            .modules
            .iter()
            .map(|module| PackageModuleInspection {
                package_id: module.package_id.clone(),
                module_path: module.module_path.clone(),
                file: module.file,
                definition_span: module.definition_span,
                source_span: module.source_span,
                function_stable_ids: module
                    .function_indices
                    .iter()
                    .filter_map(|slot| stable_by_slot.get(slot).copied())
                    .collect::<Vec<_>>()
                    .into(),
            })
            .collect::<Vec<_>>();
        let host_imports = self
            .debug_info
            .host_imports
            .iter()
            .map(|import| PackageHostImportInspection {
                stable_id: import.stable_id,
                contract_id: import.contract_id,
                contract_name: import.contract_name.clone(),
                function_name: import.function_name.clone(),
                contract_span: import.contract_span,
                declaration_span: import.declaration_span,
            })
            .collect::<Vec<_>>();
        PackageDebugInspection {
            root_package_id: self.debug_info.root_package_id.clone(),
            entry_module: self.debug_info.entry_module.clone(),
            modules: modules.into(),
            functions: functions.into(),
            host_imports: host_imports.into(),
        }
    }

    /// Number of source modules retained by the verified artifact.
    #[must_use]
    pub fn module_count(&self) -> usize {
        self.debug_info.modules.len()
    }

    /// Resolves the source symbol represented by one low-level Runtime stack frame.
    ///
    /// The frame itself is a Runtime diagnostic record. The returned inspection never exposes the
    /// dense bytecode slot used to perform this lookup.
    #[must_use]
    pub fn function_for_script_frame(
        &self,
        frame: &nexa_runtime::ScriptFrame,
    ) -> Option<PackageFunctionInspection> {
        self.debug_info
            .functions
            .iter()
            .find(|function| function.function_index == frame.function)
            .map(|function| package_function_inspection(self, function))
    }
}

fn package_function_inspection(
    artifact: &CompiledPackageArtifact,
    function: &nexa_compiler::PackageFunctionDebugInfo,
) -> PackageFunctionInspection {
    PackageFunctionInspection {
        stable_id: function.stable_id.0,
        package_id: function.package_id.clone(),
        module_path: function.module_path.clone(),
        name: function.name.clone(),
        definition_span: function.definition_span,
        signature: artifact
            .module()
            .functions
            .get(usize::try_from(function.function_index).unwrap_or(usize::MAX))
            .map(|compiled| compiled.signature.clone()),
        effect: function.effect,
        visibility: function.visibility.into(),
    }
}
