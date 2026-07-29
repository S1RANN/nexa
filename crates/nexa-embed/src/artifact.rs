use nexa_core::{SourceSpan, StableId};
use nexa_verifier::VerifiedModule;
use std::time::{Duration, Instant};

use crate::contract::ExportRequirement;
use crate::diagnostic::{EngineDiagnostic, EngineDiagnosticStage};
use crate::manifest::SourceId;
use crate::source::PackageCandidate;
use crate::source_file::SourceFileRegistry;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceHash(pub StableId);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManifestHash(pub StableId);

#[derive(Clone, Debug)]
pub struct CompiledPackageArtifact {
    pub verified: VerifiedModule,
    pub source_files: SourceFileRegistry,
    pub debug_info: ModuleDebugInfo,
    pub source_hash: SourceHash,
    pub manifest_hash: ManifestHash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleDebugInfo {
    pub module_name: String,
    pub functions: Vec<FunctionDebugInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDebugInfo {
    pub function_index: u32,
    pub name: String,
    pub stable_id: StableId,
    pub definition_span: Option<SourceSpan>,
}

#[derive(Clone, Debug)]
pub struct LastKnownGood {
    pub artifact: CompiledPackageArtifact,
    pub epoch: u64,
    pub source_hash: SourceHash,
    pub state_schema_hash: StableId,
    pub host_interface_hash: StableId,
    pub committed_generation: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct CandidateCompilation {
    pub artifact: CompiledPackageArtifact,
    pub compile_duration: Duration,
    pub verify_duration: Duration,
}

#[allow(clippy::result_large_err, clippy::too_many_lines)]
pub(crate) fn compile_package_candidate(
    idl: &nexa_idl::Idl,
    required_exports: &[ExportRequirement],
    source_id: &SourceId,
    candidate: &PackageCandidate,
) -> Result<CandidateCompilation, EngineDiagnostic> {
    let started = Instant::now();
    let package_id = candidate.manifest.id.clone();
    let entry_path = candidate.manifest.entry.as_path().to_string_lossy();
    let Some(file) = candidate.source_files.file_id(&entry_path) else {
        return Err(EngineDiagnostic::without_source(
            Some(package_id),
            Some(source_id.clone()),
            EngineDiagnosticStage::SourceDiscovery,
            nexa::ErrorCode::NX7001,
            format!("entry source is not registered: {entry_path}"),
        ));
    };
    let schema_hash = nexa_core::StableId::from_parts(&[
        candidate.manifest.id.as_str(),
        "::",
        &candidate.manifest.state_schema,
    ]);
    let compile_started = Instant::now();
    let verified =
        nexa::compile_with_interface_file(&candidate.entry_source, file, idl, schema_hash)
            .map_err(|error| match error {
                nexa::NexaError::Diagnostic(diagnostic) => EngineDiagnostic::from_leaf(
                    Some(package_id.clone()),
                    Some(source_id.clone()),
                    diagnostic_stage(diagnostic.code),
                    *diagnostic,
                    Some(&candidate.source_files),
                ),
                other => EngineDiagnostic::without_source(
                    Some(package_id.clone()),
                    Some(source_id.clone()),
                    EngineDiagnosticStage::Verify,
                    other.code(),
                    other.to_string(),
                ),
            })?;
    let compile_duration = compile_started.elapsed();
    let verify_duration = started.elapsed().saturating_sub(compile_duration);
    for requirement in required_exports {
        let Some(found) = verified
            .module()
            .exports
            .iter()
            .find(|export| export.stable_id == requirement.stable_id)
        else {
            let mut diagnostic = EngineDiagnostic::without_source(
                Some(package_id.clone()),
                Some(source_id.clone()),
                EngineDiagnosticStage::Export,
                nexa::ErrorCode::NX7010,
                format!("missing required export {}", requirement.name),
            );
            diagnostic.fixes.push(format!(
                "declare export {} with the required signature",
                requirement.name
            ));
            return Err(diagnostic);
        };
        if found.signature != requirement.signature {
            let mut diagnostic = EngineDiagnostic::without_source(
                Some(package_id.clone()),
                Some(source_id.clone()),
                EngineDiagnosticStage::Export,
                nexa::ErrorCode::NX7011,
                format!("export {} has an incompatible signature", requirement.name),
            );
            diagnostic.fixes.push(format!(
                "change export {} to the Host contract signature",
                requirement.name
            ));
            return Err(diagnostic);
        }
    }
    let tokens = nexa_compiler::lex(&candidate.entry_source).map_err(|error| {
        EngineDiagnostic::from_leaf(
            Some(package_id.clone()),
            Some(source_id.clone()),
            EngineDiagnosticStage::Parse,
            nexa::Diagnostic::new(&error, file),
            Some(&candidate.source_files),
        )
    })?;
    let ast = nexa_compiler::parse_with_file(&tokens, file).map_err(|error| {
        EngineDiagnostic::from_leaf(
            Some(package_id.clone()),
            Some(source_id.clone()),
            EngineDiagnosticStage::Parse,
            nexa::Diagnostic::new(&error, file),
            Some(&candidate.source_files),
        )
    })?;
    let functions = ast
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| FunctionDebugInfo {
            function_index: u32::try_from(index).unwrap_or(u32::MAX),
            name: function.name.clone(),
            stable_id: nexa_core::StableId::from_name(&function.name),
            definition_span: Some(function.span),
        })
        .collect();
    Ok(CandidateCompilation {
        artifact: CompiledPackageArtifact {
            verified,
            source_files: candidate.source_files.clone(),
            debug_info: ModuleDebugInfo {
                module_name: candidate.manifest.id.to_string(),
                functions,
            },
            source_hash: SourceHash(StableId::from_parts(&[
                &candidate.manifest_source,
                "\0",
                &candidate.entry_source,
            ])),
            manifest_hash: ManifestHash(candidate.manifest_hash),
        },
        compile_duration,
        verify_duration,
    })
}

fn diagnostic_stage(code: nexa::DiagnosticCode) -> EngineDiagnosticStage {
    match code.as_str().as_bytes().get(2).copied() {
        Some(b'1') => EngineDiagnosticStage::Parse,
        Some(b'2') => EngineDiagnosticStage::TypeCheck,
        Some(b'3') => EngineDiagnosticStage::Verify,
        _ => EngineDiagnosticStage::Compile,
    }
}

#[allow(clippy::result_large_err)]
pub fn compile_package(
    idl: &nexa_idl::Idl,
    required_exports: &[ExportRequirement],
    source_id: &SourceId,
    candidate: &PackageCandidate,
) -> Result<CompiledPackageArtifact, EngineDiagnostic> {
    compile_package_candidate(idl, required_exports, source_id, candidate)
        .map(|compilation| compilation.artifact)
}
