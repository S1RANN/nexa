use std::sync::Arc;
use std::time::{Duration, Instant};

use nexa::{ClassifiedError, PackageBuildError};
use nexa_analysis::{
    CandidateIdentity, PublicApiFingerprint, ResolvedBuildInput, ResolvedDependencyGraph,
    SourceSetFingerprint, StateSchemaFingerprint,
};

use crate::contract::ExportRequirement;
use crate::diagnostic::{EngineDiagnostic, EngineDiagnosticStage};
use crate::manifest::SourceId;

pub use nexa::{
    CompiledPackageArtifact, PackageDebugInspection, PackageFunctionInspection,
    PackageHostImportInspection, PackageModuleInspection,
};

#[derive(Clone, Debug)]
pub struct LastKnownGood {
    pub artifact: CompiledPackageArtifact,
    pub epoch: u64,
    pub identity: CandidateIdentity,
    pub source_set_fingerprint: SourceSetFingerprint,
    pub public_api_fingerprint: PublicApiFingerprint,
    pub state_schema_fingerprint: StateSchemaFingerprint,
    pub linked_state_fingerprint: nexa::LinkedStateFingerprint,
    pub dependency_closure: Arc<ResolvedDependencyGraph>,
    pub host_contract_fingerprint: [u8; 32],
    pub host_contract_id: nexa::StableId,
}

#[derive(Clone, Debug)]
pub struct CandidateCompilation {
    pub artifact: CompiledPackageArtifact,
    pub compile_duration: Duration,
    pub verify_duration: Duration,
}

pub(crate) struct CandidateCompilationFailure {
    pub diagnostic: EngineDiagnostic,
    pub additional_diagnostics: Vec<EngineDiagnostic>,
    pub compile_duration: Duration,
    pub verify_duration: Duration,
}

impl CandidateCompilationFailure {
    const fn new(
        diagnostic: EngineDiagnostic,
        compile_duration: Duration,
        verify_duration: Duration,
    ) -> Self {
        Self {
            diagnostic,
            additional_diagnostics: Vec::new(),
            compile_duration,
            verify_duration,
        }
    }

    fn from_diagnostics(
        mut diagnostics: Vec<EngineDiagnostic>,
        compile_duration: Duration,
        verify_duration: Duration,
    ) -> Self {
        let diagnostic = if diagnostics.is_empty() {
            EngineDiagnostic::without_source(
                None,
                None,
                EngineDiagnosticStage::Compile,
                nexa::ErrorCode::NX7001,
                "package build failed without a diagnostic",
            )
        } else {
            let primary = diagnostics
                .iter()
                .position(|diagnostic| diagnostic.stage == EngineDiagnosticStage::Parse)
                .or_else(|| {
                    diagnostics
                        .iter()
                        .position(|diagnostic| diagnostic.stage == EngineDiagnosticStage::TypeCheck)
                })
                .unwrap_or(0);
            diagnostics.remove(primary)
        };
        Self {
            diagnostic,
            additional_diagnostics: diagnostics,
            compile_duration,
            verify_duration,
        }
    }
}

#[allow(clippy::result_large_err)]
pub(crate) fn compile_package_candidate(
    build_session: &mut nexa::PackageBuildSession,
    host_contract: &nexa::HostContractInput<'_>,
    required_exports: &[ExportRequirement],
    source_id: &SourceId,
    identity: CandidateIdentity,
    build_input: &ResolvedBuildInput,
) -> Result<CandidateCompilation, CandidateCompilationFailure> {
    let package_id = identity.package_id.clone();
    let observation =
        build_session.compile_package_with_contract_observed(build_input, host_contract, identity);
    let compile_duration = observation.durations.compile_duration;
    let verify_duration = observation.durations.verify_duration;
    let artifact = observation.result.map_err(|error| {
        let diagnostics =
            package_build_diagnostics(Some(package_id.clone()), Some(source_id.clone()), error);
        CandidateCompilationFailure::from_diagnostics(
            diagnostics,
            compile_duration,
            verify_duration,
        )
    })?;

    let export_started = Instant::now();
    for requirement in required_exports {
        let Some(found) = artifact
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
                format!("missing required entrypoint {}", requirement.name),
            );
            diagnostic.fixes.push(format!(
                "declare entrypoint {} with the required signature",
                requirement.name
            ));
            return Err(CandidateCompilationFailure::new(
                diagnostic,
                compile_duration.saturating_add(export_started.elapsed()),
                verify_duration,
            ));
        };
        let found_effect = usize::try_from(found.function)
            .ok()
            .and_then(|index| artifact.module().functions.get(index))
            .map(|function| function.effect);
        if found.signature != requirement.signature
            || !found_effect
                .is_some_and(|found| crate::effect_satisfies_declaration(found, requirement.effect))
        {
            let mut diagnostic = EngineDiagnostic::without_source(
                Some(package_id.clone()),
                Some(source_id.clone()),
                EngineDiagnosticStage::Export,
                nexa::ErrorCode::NX7011,
                format!(
                    "entrypoint {} has an incompatible signature or effect",
                    requirement.name
                ),
            );
            diagnostic.fixes.push(format!(
                "change entrypoint {} to the Host contract signature and effect",
                requirement.name
            ));
            return Err(CandidateCompilationFailure::new(
                diagnostic,
                compile_duration.saturating_add(export_started.elapsed()),
                verify_duration,
            ));
        }
    }

    Ok(CandidateCompilation {
        artifact,
        compile_duration: compile_duration.saturating_add(export_started.elapsed()),
        verify_duration,
    })
}

fn package_build_diagnostics(
    package_id: Option<nexa_analysis::PackageId>,
    source_id: Option<SourceId>,
    error: PackageBuildError,
) -> Vec<EngineDiagnostic> {
    match error {
        PackageBuildError::AnalysisFailed(batch) => {
            let diagnostics = EngineDiagnostic::from_diagnostic_batch(
                package_id.clone(),
                source_id.clone(),
                EngineDiagnosticStage::TypeCheck,
                &batch,
            );
            if diagnostics.is_empty() {
                vec![EngineDiagnostic::without_source(
                    package_id,
                    source_id,
                    EngineDiagnosticStage::TypeCheck,
                    nexa::ErrorCode::NX7001,
                    "package analysis failed without a diagnostic",
                )]
            } else {
                diagnostics
            }
        }
        PackageBuildError::Verify(error) => vec![EngineDiagnostic::without_source(
            package_id,
            source_id,
            EngineDiagnosticStage::Verify,
            error.metadata().code,
            error.to_string(),
        )],
        PackageBuildError::MissingRequiredEntrypoint(name) => {
            vec![EngineDiagnostic::without_source(
                package_id,
                source_id,
                EngineDiagnosticStage::Export,
                nexa::ErrorCode::NX7010,
                format!("missing required entrypoint {name}"),
            )]
        }
        PackageBuildError::EntrypointSignatureMismatch { name, .. } => {
            vec![EngineDiagnostic::without_source(
                package_id,
                source_id,
                EngineDiagnosticStage::Export,
                nexa::ErrorCode::NX7011,
                format!("entrypoint {name} has an incompatible signature or effect"),
            )]
        }
        error => vec![EngineDiagnostic::without_source(
            package_id,
            source_id,
            EngineDiagnosticStage::Compile,
            nexa::ErrorCode::NX7001,
            error.to_string(),
        )],
    }
}

#[allow(clippy::result_large_err)]
pub fn compile_package(
    idl: &nexa::ValidatedContract,
    required_exports: &[ExportRequirement],
    source_id: &SourceId,
    identity: CandidateIdentity,
    build_input: &ResolvedBuildInput,
) -> Result<CompiledPackageArtifact, EngineDiagnostic> {
    let mut build_session = nexa::PackageBuildSession::new();
    let required_entrypoints = required_exports
        .iter()
        .map(|entrypoint| entrypoint.name.clone())
        .collect::<Vec<_>>();
    let host_contract = nexa::HostContractInput::canonical(idl)
        .requiring_entrypoints(&required_entrypoints)
        .map_err(|error| {
            EngineDiagnostic::without_source(
                Some(identity.package_id.clone()),
                Some(source_id.clone()),
                EngineDiagnosticStage::Export,
                nexa::ErrorCode::NX7011,
                error.to_string(),
            )
        })?;
    compile_package_candidate(
        &mut build_session,
        &host_contract,
        required_exports,
        source_id,
        identity,
        build_input,
    )
    .map(|compilation| compilation.artifact)
    .map_err(|failure| failure.diagnostic)
}
