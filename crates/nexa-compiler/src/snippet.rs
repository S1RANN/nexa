//! Single-source adapters over the canonical M4 package pipeline.
//!
//! These adapters deliberately do not rewrite source text. `SourceSetBuilder` supplies a virtual
//! semantic module identity when a single-file source omits `module`, and every diagnostic
//! and emitted source-map entry for that source is remapped to the caller's `FileId`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, BuildFingerprintInput, CompilationOptions, ExternalFieldSurface,
    ExternalSourceOrigin, ExternalTypeKind, ExternalTypeSurface, ExternalVariantSurface,
    HostAsyncResultSurface, HostContractSurface, HostFunctionMode, HostFunctionSurface,
    IrAbandonPolicy, IrCancelPolicy, ModulePath, NormalizedPackagePath, PackageId, PackageManifest,
    PackageSourceSet, QueryDatabase, RequiredExportSurface, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, SnippetModuleInferenceError,
    SnippetModuleInferenceErrorKind, SourceId, SourceSetBuilder, SurfaceType, analyze_package,
    external_source_key, infer_snippet_module, source_set_fingerprint,
};
use nexa_bytecode::{
    Module, ValueType, array_type, buffer_type, option_type, result_type, snapshot_type,
};
use nexa_core::{FileId, SourceSpan, StableId};
use nexa_diagnostics::{Diagnostic, ErrorCode, LabelStyle, SourceIdentity};
use nexa_idl::{AbandonPolicy, CancelPolicy, Idl, TypeRef};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

use crate::{
    AnalysisDiagnostic as CompilerAnalysisDiagnostic, AnalysisDiagnosticLabel,
    AnalysisDiagnosticSource, CompileError, compile_typed_package,
};

const SNIPPET_PACKAGE: &str = "nexa.snippet";
const SNIPPET_PATH: &str = "src/snippet.nexa";
const INTERNAL_ROOT_FILE: FileId = FileId(1);

pub(super) fn compile_verified(
    source: &str,
    file: FileId,
    interface: Option<&Idl>,
    host_hash_override: Option<StableId>,
) -> Result<VerifiedModule, CompileError> {
    let mut module = compile_module(source, file, interface)?;
    if let Some(host_hash) = host_hash_override {
        module.host_interface_hash = Some(host_hash);
    }
    verify(module, VerifierLimits::default()).map_err(|error| {
        CompileError::verify(
            error.to_string(),
            SourceSpan::new(file, 0, u32_len(source.len())),
        )
    })
}

pub(super) fn compile_module(
    source: &str,
    file: FileId,
    interface: Option<&Idl>,
) -> Result<Module, CompileError> {
    let compilation_options = CompilationOptions::default();
    let module = infer_snippet_module(source, compilation_options.limits.source_file_bytes)
        .map_err(|error| snippet_module_error(error, file))?;
    let input = resolved_snippet(source, file, &module, interface, compilation_options)?;
    let environment = interface.map_or_else(
        || Ok(AnalysisEnvironment::default()),
        |interface| {
            Ok(AnalysisEnvironment {
                host: Some(host_surface(interface, source_span(file, source))?),
                ..AnalysisEnvironment::default()
            })
        },
    )?;
    let mut queries = QueryDatabase::new();
    let mut outcome = analyze_package(&input, &environment, &mut queries);
    let ir = outcome.ir.take().ok_or_else(|| {
        analysis_error(
            outcome.diagnostics.diagnostics().first(),
            file,
            source,
            &input,
            &environment,
        )
    })?;
    let compiled = compile_typed_package(&ir).map_err(|error| remap_compile_error(error, file))?;
    let mut module = compiled.module;

    let root_functions = compiled
        .debug_info
        .modules
        .iter()
        .filter(|debug| debug.package_id == SNIPPET_PACKAGE)
        .flat_map(|debug| debug.function_indices.iter().copied())
        .collect::<BTreeSet<_>>();
    for entry in &mut module.source_map {
        if root_functions.contains(&entry.function) {
            entry.span.file = file;
        }
    }
    Ok(module)
}

fn snippet_module_error(error: SnippetModuleInferenceError, file: FileId) -> CompileError {
    let span = SourceSpan::new(file, error.range.start, error.range.end);
    match error.kind {
        SnippetModuleInferenceErrorKind::InvalidModulePath { .. } => {
            CompileError::AnalysisDiagnostic(Box::new(CompilerAnalysisDiagnostic {
                code: ErrorCode::NX2701,
                message: error.message,
                primary: AnalysisDiagnosticLabel {
                    source: AnalysisDiagnosticSource::Caller,
                    span,
                    message: "module path is not canonical".into(),
                },
                secondary: Vec::new(),
                related: Vec::new(),
                notes: Vec::new(),
            }))
        }
        SnippetModuleInferenceErrorKind::SourceTooLarge { .. } => {
            CompileError::unknown_name(error.message, span)
        }
    }
}

fn resolved_snippet(
    source: &str,
    file: FileId,
    module: &ModulePath,
    interface: Option<&Idl>,
    compilation_options: CompilationOptions,
) -> Result<ResolvedBuildInput, CompileError> {
    let package =
        PackageId::new(SNIPPET_PACKAGE).expect("the compiler snippet package ID is valid");
    let manifest_source = format!(
        "schema = 2\n\
         kind = \"application\"\n\
         id = \"{SNIPPET_PACKAGE}\"\n\
         name = \"Single-file snippet\"\n\
         version = \"1.0.0\"\n\
         source_root = \"src\"\n\
         entry = \"{}\"\n\
         activation = \"default-enabled\"\n",
        module.as_str()
    );
    let manifest = Arc::new(PackageManifest::parse(&manifest_source).map_err(|error| {
        CompileError::unknown_name(
            format!("invalid virtual snippet manifest: {error}"),
            SourceSpan::default(),
        )
    })?);
    let mut sources = SourceSetBuilder::new(package.clone(), compilation_options.limits);
    sources
        .add_virtual_snippet(
            NormalizedPackagePath::new(SNIPPET_PATH)
                .expect("the compiler snippet source path is normalized"),
            Arc::<str>::from(source),
            module.clone(),
        )
        .map_err(|error| {
            CompileError::unknown_name(
                format!("invalid virtual snippet source: {error}"),
                SourceSpan::new(file, 0, u32_len(source.len())),
            )
        })?;
    let sources = Arc::new(sources.build().map_err(|error| {
        CompileError::unknown_name(
            format!("invalid virtual snippet source set: {error}"),
            SourceSpan::new(file, 0, u32_len(source.len())),
        )
    })?);
    let graph = Arc::new(ResolvedDependencyGraph {
        root: package.clone(),
        packages: BTreeMap::from([(
            package.clone(),
            ResolvedPackage {
                id: package.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("compiler-virtual-snippet")
                    .expect("the compiler virtual source ID is valid"),
                directory: NormalizedPackagePath::new("virtual/snippet")
                    .expect("the compiler virtual package path is normalized"),
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let canonical_host_contract =
        interface.map_or_else(Vec::new, |idl| nexa_idl::canonical(idl).into_bytes());
    let canonical_host_required_exports = interface.map_or_else(
        || nexa_idl::canonical_required_exports(std::iter::empty::<&str>()),
        nexa_idl::canonical_all_required_exports,
    );
    let fingerprint = snippet_build_fingerprint(
        package,
        &manifest,
        &sources,
        &canonical_host_contract,
        &canonical_host_required_exports,
        &compilation_options,
    );
    ResolvedBuildInput::new(
        manifest,
        sources,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        canonical_host_contract,
        Vec::<u8>::new(),
        canonical_host_required_exports,
        compilation_options,
        fingerprint,
    )
    .map_err(|error| {
        CompileError::unknown_name(
            format!("invalid virtual snippet build input: {error}"),
            SourceSpan::new(file, 0, u32_len(source.len())),
        )
    })
}

fn snippet_build_fingerprint(
    package: PackageId,
    manifest: &PackageManifest,
    sources: &PackageSourceSet,
    canonical_host_contract: &[u8],
    canonical_host_required_exports: &[u8],
    compilation_options: &CompilationOptions,
) -> BuildFingerprintInput {
    let standard_library = nexa_stdlib::standard_library();
    BuildFingerprintInput {
        root_package: package,
        root_manifest: manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(sources),
        dependency_manifests: BTreeMap::new(),
        dependency_source_sets: BTreeMap::new(),
        host_contract: canonical_host_contract.to_vec(),
        host_contract_source: Vec::new(),
        host_required_exports: canonical_host_required_exports.to_vec(),
        language_version: nexa_analysis::NEXA_LANGUAGE_VERSION.into(),
        standard_library_version: standard_library.version.to_string(),
        standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
        compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
        compiler_options: nexa_analysis::canonical_compilation_options(compilation_options),
        canonical_lock_graph: Vec::new(),
    }
}

#[allow(clippy::too_many_lines)]
fn host_surface(idl: &Idl, span: SourceSpan) -> Result<HostContractSurface, CompileError> {
    let host_module = ModulePath::new("host").expect("host is a valid reserved module");
    let mut types = Vec::new();
    for name in &idl.opaque_handles {
        types.push(ExternalTypeSurface {
            name: name.clone(),
            kind: ExternalTypeKind::Opaque,
            stable_id: Some(StableId::from_name(name)),
            type_parameters: Vec::new(),
            fields: Vec::new(),
            variants: Vec::new(),
            source: None,
        });
    }
    for structure in &idl.structs {
        let fields = structure
            .fields
            .iter()
            .map(|field| {
                Ok(ExternalFieldSurface {
                    name: field.name.clone(),
                    stable_id: Some(StableId::from_parts(&[&structure.name, "::", &field.name])),
                    ty: surface_type(&field.ty, &host_module)?,
                    source: None,
                })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;
        types.push(ExternalTypeSurface {
            name: structure.name.clone(),
            kind: ExternalTypeKind::Struct,
            stable_id: Some(StableId::from_name(&structure.name)),
            type_parameters: Vec::new(),
            fields,
            variants: Vec::new(),
            source: None,
        });
    }
    for enumeration in &idl.enums {
        let variants = enumeration
            .variants
            .iter()
            .map(|variant| {
                Ok(ExternalVariantSurface {
                    name: variant.name.clone(),
                    stable_id: Some(StableId::from_parts(&[
                        &enumeration.name,
                        "::",
                        &variant.name,
                    ])),
                    payload: variant
                        .payload
                        .as_ref()
                        .map(|payload| surface_type(payload, &host_module))
                        .transpose()?
                        .into_iter()
                        .collect(),
                    source: None,
                })
            })
            .collect::<Result<Vec<_>, CompileError>>()?;
        types.push(ExternalTypeSurface {
            name: enumeration.name.clone(),
            kind: ExternalTypeKind::Enum,
            stable_id: Some(StableId::from_name(&enumeration.name)),
            type_parameters: Vec::new(),
            fields: Vec::new(),
            variants,
            source: None,
        });
    }

    let functions = idl
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let import_index =
                u32::try_from(index).map_err(|_| CompileError::too_many_registers(span))?;
            let parameters = function
                .parameters
                .iter()
                .map(|parameter| surface_type(&parameter.ty, &host_module))
                .collect::<Result<Vec<_>, _>>()?;
            let result = surface_type(&function.result, &host_module)?;
            let (mode, async_result) = if function.synchronous {
                (HostFunctionMode::Sync, None)
            } else {
                (
                    HostFunctionMode::Request,
                    Some(async_result_surface(idl, function, &host_module, span)?),
                )
            };
            Ok(HostFunctionSurface {
                name: function.name.clone(),
                parameters,
                result,
                mode,
                stable_id: StableId::from_parts(&[&idl.interface, "::", &function.name]),
                import_index,
                fuel_cost: function.fuel_cost,
                async_result,
                required_capability: None,
                source: None,
            })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
    let required_exports = idl
        .exports
        .iter()
        .map(|export| {
            Ok(RequiredExportSurface {
                name: export.name.clone(),
                stable_id: nexa_idl::export_stable_id(idl, export),
                parameters: export
                    .parameters
                    .iter()
                    .map(|parameter| surface_type(&parameter.ty, &host_module))
                    .collect::<Result<Vec<_>, _>>()?,
                result: export
                    .result
                    .as_ref()
                    .map(|result| surface_type(result, &host_module))
                    .transpose()?
                    .unwrap_or(SurfaceType::Unit),
                effect: None,
                source: None,
            })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
    Ok(HostContractSurface {
        interface_name: idl.interface.clone(),
        interface_stable_id: nexa_idl::exact_hash(idl),
        types,
        functions,
        required_exports,
        source: None,
    })
}

fn async_result_surface(
    idl: &Idl,
    function: &nexa_idl::HostFunction,
    host_module: &ModulePath,
    span: SourceSpan,
) -> Result<HostAsyncResultSurface, CompileError> {
    let TypeRef::HostRequest(Some(request)) = &function.result else {
        return Err(CompileError::type_mismatch(None, None, span));
    };
    let TypeRef::Result(success, error) = request.as_ref() else {
        return Err(CompileError::type_mismatch(None, None, span));
    };
    let success_value = idl_value_type(success, span)?;
    let error_value = idl_value_type(error, span)?;
    Ok(HostAsyncResultSurface {
        result_type: result_type(success_value, error_value).type_id,
        success: surface_type(success, host_module)?,
        error: surface_type(error, host_module)?,
        cancel_policy: match function.cancel_policy {
            CancelPolicy::ReturnError => IrCancelPolicy::ReturnError,
            CancelPolicy::CancelTask => IrCancelPolicy::CancelTask,
        },
        abandon_policy: match function.abandon_policy {
            AbandonPolicy::ReturnError => IrAbandonPolicy::ReturnError,
            AbandonPolicy::Trap => IrAbandonPolicy::Trap,
        },
        cancel_error: match function.cancel_policy {
            CancelPolicy::ReturnError => Some(policy_error_tag(
                idl,
                error,
                "Cancelled",
                u32::MAX - 1,
                span,
            )?),
            CancelPolicy::CancelTask => None,
        },
        abandon_error: match function.abandon_policy {
            AbandonPolicy::ReturnError => {
                Some(policy_error_tag(idl, error, "Abandoned", u32::MAX, span)?)
            }
            AbandonPolicy::Trap => None,
        },
    })
}

fn policy_error_tag(
    idl: &Idl,
    error: &TypeRef,
    variant: &str,
    integer_fallback: u32,
    span: SourceSpan,
) -> Result<u32, CompileError> {
    match error {
        TypeRef::I32 => Ok(integer_fallback),
        TypeRef::Named(name) => {
            idl.enums
                .iter()
                .find(|enumeration| enumeration.name == *name)
                .and_then(|enumeration| {
                    enumeration.variants.iter().position(|candidate| {
                        candidate.name == variant && candidate.payload.is_none()
                    })
                })
                .and_then(|index| u32::try_from(index).ok())
                .ok_or_else(|| CompileError::type_mismatch(None, None, span))
        }
        _ => Err(CompileError::type_mismatch(None, None, span)),
    }
}

fn surface_type(ty: &TypeRef, named_module: &ModulePath) -> Result<SurfaceType, CompileError> {
    Ok(match ty {
        TypeRef::I32 => SurfaceType::I32,
        TypeRef::I64 => SurfaceType::I64,
        TypeRef::F32 => SurfaceType::F32,
        TypeRef::F64 => SurfaceType::F64,
        TypeRef::Bool => SurfaceType::Bool,
        TypeRef::Rune => SurfaceType::Rune,
        TypeRef::String => SurfaceType::String,
        TypeRef::HostRequest(inner) => SurfaceType::HostRequest(
            inner
                .as_ref()
                .map(|inner| surface_type(inner, named_module).map(Box::new))
                .transpose()?,
        ),
        TypeRef::ResourceToken(inner) => SurfaceType::ResourceToken(
            inner
                .as_ref()
                .map(|inner| surface_type(inner, named_module).map(Box::new))
                .transpose()?,
        ),
        TypeRef::Snapshot(Some(inner)) => {
            SurfaceType::Snapshot(Box::new(surface_type(inner, named_module)?))
        }
        TypeRef::Snapshot(None) => {
            return Err(CompileError::type_mismatch(
                None,
                None,
                SourceSpan::default(),
            ));
        }
        TypeRef::Array(inner) => SurfaceType::Array(Box::new(surface_type(inner, named_module)?)),
        TypeRef::Buffer(inner) => SurfaceType::Buffer(Box::new(surface_type(inner, named_module)?)),
        TypeRef::Option(inner) => SurfaceType::Option(Box::new(surface_type(inner, named_module)?)),
        TypeRef::Result(success, error) => SurfaceType::Result(
            Box::new(surface_type(success, named_module)?),
            Box::new(surface_type(error, named_module)?),
        ),
        TypeRef::Named(name) => SurfaceType::Named {
            module: named_module.clone(),
            name: name.clone(),
        },
    })
}

fn idl_value_type(ty: &TypeRef, span: SourceSpan) -> Result<ValueType, CompileError> {
    Ok(match ty {
        TypeRef::I32 => ValueType::I32,
        TypeRef::I64 => ValueType::I64,
        TypeRef::F32 => ValueType::F32,
        TypeRef::F64 => ValueType::F64,
        TypeRef::Bool => ValueType::Bool,
        TypeRef::Rune => ValueType::Rune,
        TypeRef::String => ValueType::String,
        TypeRef::HostRequest(_) => ValueType::Named(StableId::from_name("HostRequest")),
        TypeRef::ResourceToken(_) => ValueType::Named(StableId::from_name("ResourceToken")),
        TypeRef::Snapshot(Some(inner)) => {
            let ValueType::Named(content) = idl_value_type(inner, span)? else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            ValueType::Named(snapshot_type(content))
        }
        TypeRef::Snapshot(None) => return Err(CompileError::type_mismatch(None, None, span)),
        TypeRef::Array(inner) => ValueType::Named(array_type(idl_value_type(inner, span)?)),
        TypeRef::Buffer(inner) => ValueType::Named(buffer_type(idl_value_type(inner, span)?)),
        TypeRef::Option(inner) => {
            ValueType::Named(option_type(idl_value_type(inner, span)?).type_id)
        }
        TypeRef::Result(success, error) => ValueType::Named(
            result_type(idl_value_type(success, span)?, idl_value_type(error, span)?).type_id,
        ),
        TypeRef::Named(name) => ValueType::Named(StableId::from_name(name)),
    })
}

fn analysis_error(
    diagnostic: Option<&Diagnostic>,
    file: FileId,
    source: &str,
    input: &ResolvedBuildInput,
    environment: &AnalysisEnvironment,
) -> CompileError {
    let fallback = SourceSpan::new(file, 0, u32_len(source.len()));
    let Some(diagnostic) = diagnostic else {
        return CompileError::unknown_name("analysis produced no typed IR".into(), fallback);
    };
    let primary = diagnostic.primary_label().map_or(
        AnalysisDiagnosticLabel {
            source: AnalysisDiagnosticSource::Caller,
            span: fallback,
            message: "primary source location".into(),
        },
        |label| {
            analysis_label(
                &label.source,
                label.range.start,
                label.range.end,
                &label.message,
                file,
                input,
                environment,
            )
        },
    );
    let secondary = diagnostic
        .labels
        .iter()
        .filter(|label| label.style == LabelStyle::Secondary)
        .map(|label| {
            analysis_label(
                &label.source,
                label.range.start,
                label.range.end,
                &label.message,
                file,
                input,
                environment,
            )
        })
        .collect();
    let related = diagnostic
        .related
        .iter()
        .map(|location| {
            analysis_label(
                &location.source,
                location.range.start,
                location.range.end,
                &location.message,
                file,
                input,
                environment,
            )
        })
        .collect();
    CompileError::AnalysisDiagnostic(Box::new(CompilerAnalysisDiagnostic {
        code: diagnostic.code,
        message: diagnostic.message.to_string(),
        primary,
        secondary,
        related,
        notes: diagnostic.notes.iter().map(ToString::to_string).collect(),
    }))
}

fn analysis_label(
    identity: &SourceIdentity,
    start: u32,
    end: u32,
    message: &str,
    caller_file: FileId,
    input: &ResolvedBuildInput,
    environment: &AnalysisEnvironment,
) -> AnalysisDiagnosticLabel {
    let is_root = identity.package_id() == Some(SNIPPET_PACKAGE) && identity.path() == SNIPPET_PATH;
    let (source, file) = if is_root {
        (AnalysisDiagnosticSource::Caller, caller_file)
    } else {
        (
            AnalysisDiagnosticSource::Canonical(identity.clone()),
            candidate_file_id(identity, input, environment),
        )
    };
    AnalysisDiagnosticLabel {
        source,
        span: SourceSpan::new(file, start, end),
        message: message.to_owned(),
    }
}

fn candidate_file_id(
    identity: &SourceIdentity,
    input: &ResolvedBuildInput,
    environment: &AnalysisEnvironment,
) -> FileId {
    if let Some(id) = input.artifact_files.id_for(&external_source_key(identity)) {
        return FileId(id.0);
    }

    let mut standard_sources = nexa_stdlib::standard_library()
        .modules()
        .iter()
        .map(|descriptor| {
            SourceIdentity::package(
                nexa_stdlib::PACKAGE_ID,
                format!("stdlib/{}.nexa", descriptor.path.replace('.', "/")),
            )
        })
        .collect::<Vec<_>>();
    standard_sources.sort();
    if let Some(offset) = standard_sources
        .iter()
        .position(|candidate| candidate == identity)
    {
        return file_id_after(input.artifact_files.files().len(), offset);
    }

    let external_sources = external_source_identities(environment);
    if let Some(offset) = external_sources
        .iter()
        .position(|candidate| candidate == identity)
    {
        return file_id_after(
            input
                .artifact_files
                .files()
                .len()
                .saturating_add(standard_sources.len()),
            offset,
        );
    }
    FileId(u32::MAX)
}

fn file_id_after(base: usize, offset: usize) -> FileId {
    FileId(
        base.checked_add(offset)
            .and_then(|value| value.checked_add(1))
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(u32::MAX),
    )
}

fn external_source_identities(environment: &AnalysisEnvironment) -> Vec<SourceIdentity> {
    let mut identities = BTreeSet::new();
    if let Some(host) = &environment.host {
        insert_external_origin(&mut identities, host.source.as_ref());
        for function in &host.functions {
            insert_external_origin(&mut identities, function.source.as_ref());
        }
        for ty in &host.types {
            insert_external_type_origins(&mut identities, ty);
        }
    }
    for module in &environment.static_modules {
        for ty in &module.types {
            insert_external_type_origins(&mut identities, ty);
        }
    }
    identities.into_iter().collect()
}

fn insert_external_type_origins(
    identities: &mut BTreeSet<SourceIdentity>,
    ty: &ExternalTypeSurface,
) {
    insert_external_origin(identities, ty.source.as_ref());
    for field in &ty.fields {
        insert_external_origin(identities, field.source.as_ref());
    }
    for variant in &ty.variants {
        insert_external_origin(identities, variant.source.as_ref());
    }
}

fn insert_external_origin(
    identities: &mut BTreeSet<SourceIdentity>,
    origin: Option<&ExternalSourceOrigin>,
) {
    if let Some(origin) = origin {
        identities.insert(origin.identity.clone());
    }
}

#[allow(clippy::too_many_lines)]
fn remap_compile_error(error: CompileError, file: FileId) -> CompileError {
    let remap = |span: SourceSpan| {
        if span.file == INTERNAL_ROOT_FILE {
            SourceSpan::new(file, span.start, span.end)
        } else {
            span
        }
    };
    match error {
        CompileError::AnalysisDiagnostic(diagnostic) => {
            CompileError::AnalysisDiagnostic(Box::new(CompilerAnalysisDiagnostic {
                code: diagnostic.code,
                message: diagnostic.message,
                primary: remap_analysis_label(diagnostic.primary, file),
                secondary: diagnostic
                    .secondary
                    .into_iter()
                    .map(|label| remap_analysis_label(label, file))
                    .collect(),
                related: diagnostic
                    .related
                    .into_iter()
                    .map(|label| remap_analysis_label(label, file))
                    .collect(),
                notes: diagnostic.notes,
            }))
        }
        CompileError::DuplicateName {
            name,
            first,
            duplicate,
        } => CompileError::DuplicateName {
            name,
            first: remap(first),
            duplicate: remap(duplicate),
        },
        CompileError::UnknownName { name, span } => CompileError::UnknownName {
            name,
            span: remap(span),
        },
        CompileError::UnknownType { name, span } => CompileError::UnknownType {
            name,
            span: remap(span),
        },
        CompileError::TypeMismatch {
            expected,
            actual,
            span,
        } => CompileError::TypeMismatch {
            expected,
            actual,
            span: remap(span),
        },
        CompileError::MissingReturn { function_span } => CompileError::MissingReturn {
            function_span: remap(function_span),
        },
        CompileError::DeferCaptureLimit { span } => {
            CompileError::DeferCaptureLimit { span: remap(span) }
        }
        CompileError::InvalidEffect { span } => CompileError::InvalidEffect { span: remap(span) },
        CompileError::InvalidReloadMetadata {
            message,
            function_span,
        } => CompileError::InvalidReloadMetadata {
            message,
            function_span: remap(function_span),
        },
        CompileError::TooManyRegisters { function_span } => CompileError::TooManyRegisters {
            function_span: remap(function_span),
        },
        CompileError::Verify {
            message,
            function_span,
        } => CompileError::Verify {
            message,
            function_span: remap(function_span),
        },
    }
}

fn remap_analysis_label(
    label: AnalysisDiagnosticLabel,
    caller_file: FileId,
) -> AnalysisDiagnosticLabel {
    let AnalysisDiagnosticLabel {
        source,
        span,
        message,
    } = label;
    let span = match &source {
        AnalysisDiagnosticSource::Caller if span.file == INTERNAL_ROOT_FILE => {
            SourceSpan::new(caller_file, span.start, span.end)
        }
        AnalysisDiagnosticSource::Caller | AnalysisDiagnosticSource::Canonical(_) => span,
    };
    AnalysisDiagnosticLabel {
        source,
        span,
        message,
    }
}

fn source_span(file: FileId, source: &str) -> SourceSpan {
    SourceSpan::new(file, 0, u32_len(source.len()))
}

fn u32_len(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use nexa_diagnostics::{
        ByteRange, Diagnostic, ErrorCode, Label, RelatedLocation, Severity, SourceIdentity,
    };

    use crate::{AnalysisDiagnosticSource, CompileError};

    use super::{
        AnalysisEnvironment, CompilationOptions, FileId, ModulePath, analysis_error,
        resolved_snippet,
    };

    #[test]
    fn virtual_snippet_fingerprint_uses_canonical_build_authorities() {
        let options = CompilationOptions::default();
        let module = ModulePath::new("main").unwrap();
        let input = resolved_snippet(
            "module main;\nfn main() -> i32 { return 0; }\n",
            FileId(8),
            &module,
            None,
            options,
        )
        .unwrap();
        let fingerprint = input.fingerprint_input.as_ref();
        let standard_library = nexa_stdlib::standard_library();

        assert_eq!(
            fingerprint.runtime_semantics_version,
            u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION)
        );
        assert_eq!(
            fingerprint.opcode_cost_table_version,
            nexa_core::OPCODE_COST_TABLE_VERSION
        );
        assert_eq!(
            fingerprint.deterministic_math_backend,
            nexa_core::RUNTIME_MATH_BACKEND_ID
        );
        assert_eq!(
            fingerprint.compiler_options,
            nexa_analysis::canonical_compilation_options(&options)
        );
        assert_eq!(
            fingerprint.language_version,
            nexa_analysis::NEXA_LANGUAGE_VERSION
        );
        assert_eq!(
            fingerprint.compiler_version,
            nexa_core::NEXA_COMPILER_VERSION
        );
        assert_eq!(
            fingerprint.bytecode_version,
            u32::from(nexa_core::BYTECODE_VERSION)
        );
        assert_eq!(
            fingerprint.standard_library_version,
            standard_library.version.to_string()
        );
        assert_eq!(
            fingerprint.standard_library_descriptor,
            nexa_stdlib::canonical_descriptor_identity()
        );
        assert!(fingerprint.host_contract.is_empty());
        let empty_required_exports =
            nexa_idl::canonical_required_exports(std::iter::empty::<&str>());
        assert_eq!(fingerprint.host_required_exports, empty_required_exports);
        assert_eq!(
            input.host_required_exports_identity.as_ref(),
            empty_required_exports
        );
        assert!(fingerprint.canonical_lock_graph.is_empty());

        let idl = nexa_idl::parse("interface Host { export Update(value: i32) -> i32; }").unwrap();
        let hosted = resolved_snippet(
            "module main;\npub fn Update(value: i32) -> i32 { return value; }\n",
            FileId(9),
            &module,
            Some(&idl),
            options,
        )
        .unwrap();
        assert_eq!(
            hosted.fingerprint_input.host_required_exports,
            nexa_idl::canonical_all_required_exports(&idl)
        );
        assert_eq!(
            hosted.host_required_exports_identity.as_ref(),
            nexa_idl::canonical_all_required_exports(&idl)
        );
    }

    #[test]
    fn virtual_snippet_adapter_never_relabels_compiler_sources_as_the_caller() {
        let options = CompilationOptions::default();
        let module = ModulePath::new("main").unwrap();
        let source = "fn bad() -> i32 { return missing; }\n";
        let input =
            resolved_snippet(source, FileId(73), &module, None, options).expect("snippet input");
        let root = SourceIdentity::package(super::SNIPPET_PACKAGE, super::SNIPPET_PATH);
        let standard = SourceIdentity::package(nexa_stdlib::PACKAGE_ID, "stdlib/std/core.nexa");
        let diagnostic = Diagnostic::new(
            ErrorCode::NX2705,
            Severity::Error,
            "cross-source diagnostic",
        )
        .with_label(Label::primary(root, ByteRange::new(25, 32), "caller use"))
        .with_related(RelatedLocation::new(
            standard.clone(),
            ByteRange::new(3, 9),
            "compiler declaration",
        ));

        let error = analysis_error(
            Some(&diagnostic),
            FileId(73),
            source,
            &input,
            &AnalysisEnvironment::default(),
        );
        let CompileError::AnalysisDiagnostic(diagnostic) = error else {
            panic!("canonical analyzer diagnostic expected");
        };
        assert_eq!(diagnostic.primary.source, AnalysisDiagnosticSource::Caller);
        assert_eq!(diagnostic.primary.span.file, FileId(73));
        assert_eq!(
            diagnostic.related[0].source,
            AnalysisDiagnosticSource::Canonical(standard)
        );
        assert_ne!(diagnostic.related[0].span.file, FileId(73));
        assert_eq!(
            (
                diagnostic.related[0].span.start,
                diagnostic.related[0].span.end
            ),
            (3, 9)
        );
    }
}
