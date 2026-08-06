//! Canonical adapters from one exact NIDL v2 Contract into shared semantic analysis.
//!
//! The façade reparses the retained source snapshot so every external origin refers to the exact
//! URI/text supplied by the caller. Semantic identity comes from ABI Descriptor v2, never from a
//! formatted source string.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, ExternalFieldSurface, ExternalSourceOrigin, ExternalTypeKind,
    ExternalTypeSurface, ExternalVariantSurface, HostAsyncResultSurface, HostContractSurface,
    HostFunctionMode, HostFunctionSurface, IrAbandonPolicy, IrCancelPolicy, ModulePath,
    NexaEntrypointSurface, RequiredEntrypointSurface, SurfaceType,
};
use nexa_bytecode::result_type;
use nexa_core::SourceSpan;
use nexa_diagnostics::{ByteRange, SourceIdentity};
use nexa_contract::{
    AbandonPolicy, CancelPolicy, ResolvedTypeKind, ResolvedTypeRef, ValidatedContract,
    ValidatedFunction,
};

use crate::package_build::HostContractInput;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageEnvironmentError {
    TooManyHostFunctions,
    InvalidAsyncResult(String),
    MissingPolicyErrorVariant {
        function: String,
        variant: &'static str,
    },
    InvalidHostSource(nexa_contract::ContractError),
    HostSourceContractMismatch,
}

impl fmt::Display for PackageEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyHostFunctions => formatter.write_str("too many Host functions"),
            Self::InvalidAsyncResult(function) => write!(
                formatter,
                "async Host function `{function}` must return Result<Success, Error>"
            ),
            Self::MissingPolicyErrorVariant { function, variant } => write!(
                formatter,
                "async Host function `{function}` requires a zero-payload `{variant}` error variant"
            ),
            Self::InvalidHostSource(error) => write!(formatter, "invalid Host source: {error}"),
            Self::HostSourceContractMismatch => formatter.write_str(
                "exact Host source does not match the supplied validated Contract descriptor",
            ),
        }
    }
}

impl std::error::Error for PackageEnvironmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidHostSource(error) => Some(error),
            _ => None,
        }
    }
}

/// Builds the canonical environment used by product, test, standalone, and tooling pipelines.
pub(crate) fn canonical_analysis_environment(
    contract: &HostContractInput<'_>,
) -> Result<AnalysisEnvironment, PackageEnvironmentError> {
    Ok(AnalysisEnvironment {
        host: Some(canonical_host_surface(contract)?),
        ..AnalysisEnvironment::default()
    })
}

/// Converts one exact, already-validated NIDL v2 source into the only Host semantic surface.
#[allow(clippy::too_many_lines)]
pub(crate) fn canonical_host_surface(
    input: &HostContractInput<'_>,
) -> Result<HostContractSurface, PackageEnvironmentError> {
    let source = Arc::clone(input.source().text());
    let identity = input.source().identity().clone();
    let contract = nexa_contract::parse_contract(&source).map_err(PackageEnvironmentError::InvalidHostSource)?;
    if nexa_contract::abi_descriptor(&contract).bytes != nexa_contract::abi_descriptor(input.contract()).bytes
    {
        return Err(PackageEnvironmentError::HostSourceContractMismatch);
    }

    let host_module = ModulePath::new("host").expect("host is a reserved module path");
    let types = contract_types(
        &contract,
        &input.effective_selection().referenced_types,
        &host_module,
        &source,
        &identity,
    )?;
    let mut host_functions = contract
        .host_functions
        .iter()
        .filter(|function| {
            input
                .effective_selection()
                .host_functions
                .contains(&function.name)
        })
        .collect::<Vec<_>>();
    host_functions.sort_by(|left, right| {
        left.stable_id
            .cmp(&right.stable_id)
            .then_with(|| left.name.cmp(&right.name))
    });
    let functions = host_functions
        .into_iter()
        .enumerate()
        .map(|(index, function)| {
            host_function_surface(&contract, function, index, &host_module, &source, &identity)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut nexa_functions = contract.nexa_functions.iter().collect::<Vec<_>>();
    nexa_functions.sort_by(|left, right| {
        left.stable_id
            .cmp(&right.stable_id)
            .then_with(|| left.name.cmp(&right.name))
    });
    let nexa_entrypoints = nexa_functions
        .iter()
        .copied()
        .map(|entrypoint| nexa_entrypoint_surface(entrypoint, &host_module, &source, &identity))
        .collect::<Result<Vec<_>, _>>()?;
    let required_names = input
        .required_entrypoints()
        .map(|entrypoint| entrypoint.name.as_str())
        .collect::<BTreeSet<_>>();
    let required_entrypoints = nexa_functions
        .iter()
        .copied()
        .filter(|entrypoint| required_names.contains(entrypoint.name.as_str()))
        .map(|entrypoint| required_entrypoint_surface(entrypoint, &host_module, &source, &identity))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(HostContractSurface {
        contract_name: contract.name.clone(),
        contract_stable_id: input.runtime_id(),
        types,
        functions,
        nexa_entrypoints,
        required_entrypoints,
        source: Some(declaration_origin(&source, &identity, contract.span)),
    })
}

fn contract_types(
    contract: &ValidatedContract,
    selected: &BTreeSet<String>,
    host_module: &ModulePath,
    source: &Arc<str>,
    identity: &SourceIdentity,
) -> Result<Vec<ExternalTypeSurface>, PackageEnvironmentError> {
    let mut types =
        Vec::with_capacity(contract.handles.len() + contract.structs.len() + contract.enums.len());
    for handle in contract
        .handles
        .iter()
        .filter(|handle| selected.contains(&handle.name))
    {
        types.push(ExternalTypeSurface {
            name: handle.name.clone(),
            kind: ExternalTypeKind::Opaque,
            stable_id: Some(handle.stable_id),
            type_parameters: Vec::new(),
            fields: Vec::new(),
            variants: Vec::new(),
            source: Some(declaration_origin(source, identity, handle.span)),
        });
    }
    for structure in contract
        .structs
        .iter()
        .filter(|structure| selected.contains(&structure.name))
    {
        let fields = structure
            .fields
            .iter()
            .map(|field| {
                Ok(ExternalFieldSurface {
                    name: field.name.clone(),
                    stable_id: Some(field.stable_id),
                    ty: surface_type(&field.ty, host_module)?,
                    source: Some(declaration_origin(source, identity, field.span)),
                })
            })
            .collect::<Result<Vec<_>, PackageEnvironmentError>>()?;
        types.push(ExternalTypeSurface {
            name: structure.name.clone(),
            kind: ExternalTypeKind::Struct,
            stable_id: Some(structure.stable_id),
            type_parameters: Vec::new(),
            fields,
            variants: Vec::new(),
            source: Some(declaration_origin(source, identity, structure.span)),
        });
    }
    for enumeration in contract
        .enums
        .iter()
        .filter(|enumeration| selected.contains(&enumeration.name))
    {
        let variants = enumeration
            .variants
            .iter()
            .map(|variant| {
                Ok(ExternalVariantSurface {
                    name: variant.name.clone(),
                    stable_id: Some(variant.stable_id),
                    payload: variant
                        .payload
                        .as_ref()
                        .map(|payload| surface_type(payload, host_module))
                        .transpose()?
                        .into_iter()
                        .collect(),
                    source: Some(declaration_origin(source, identity, variant.span)),
                })
            })
            .collect::<Result<Vec<_>, PackageEnvironmentError>>()?;
        types.push(ExternalTypeSurface {
            name: enumeration.name.clone(),
            kind: ExternalTypeKind::Enum,
            stable_id: Some(enumeration.stable_id),
            type_parameters: Vec::new(),
            fields: Vec::new(),
            variants,
            source: Some(declaration_origin(source, identity, enumeration.span)),
        });
    }
    types.sort_by(|left, right| {
        left.stable_id
            .cmp(&right.stable_id)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(types)
}

fn host_function_surface(
    contract: &ValidatedContract,
    function: &ValidatedFunction,
    index: usize,
    host_module: &ModulePath,
    source: &Arc<str>,
    identity: &SourceIdentity,
) -> Result<HostFunctionSurface, PackageEnvironmentError> {
    let import_index =
        u32::try_from(index).map_err(|_| PackageEnvironmentError::TooManyHostFunctions)?;
    let parameters = function
        .parameters
        .iter()
        .map(|parameter| surface_type(&parameter.ty, host_module))
        .collect::<Result<Vec<_>, _>>()?;
    let result = function
        .result
        .as_ref()
        .map(|result| surface_type(result, host_module))
        .transpose()?
        .unwrap_or(SurfaceType::Unit);
    let (mode, async_result) = if function.is_async {
        (
            HostFunctionMode::Request,
            Some(async_result_surface(contract, function, host_module)?),
        )
    } else {
        (HostFunctionMode::Sync, None)
    };
    Ok(HostFunctionSurface {
        name: function.name.clone(),
        parameters,
        result,
        mode,
        stable_id: function.stable_id,
        declaration_fingerprint: function.declaration_fingerprint.into_bytes(),
        import_index,
        fuel_cost: function.fuel_cost,
        async_result,
        required_capabilities: function.capabilities.clone(),
        source: Some(declaration_origin(source, identity, function.span)),
    })
}

fn nexa_entrypoint_surface(
    entrypoint: &ValidatedFunction,
    host_module: &ModulePath,
    source: &Arc<str>,
    identity: &SourceIdentity,
) -> Result<NexaEntrypointSurface, PackageEnvironmentError> {
    Ok(NexaEntrypointSurface {
        name: entrypoint.name.clone(),
        stable_id: entrypoint.stable_id,
        parameters: entrypoint
            .parameters
            .iter()
            .map(|parameter| surface_type(&parameter.ty, host_module))
            .collect::<Result<Vec<_>, _>>()?,
        result: entrypoint
            .result
            .as_ref()
            .map(|result| surface_type(result, host_module))
            .transpose()?
            .unwrap_or(SurfaceType::Unit),
        effect: None,
        source: Some(declaration_origin(source, identity, entrypoint.span)),
    })
}

fn required_entrypoint_surface(
    entrypoint: &ValidatedFunction,
    host_module: &ModulePath,
    source: &Arc<str>,
    identity: &SourceIdentity,
) -> Result<RequiredEntrypointSurface, PackageEnvironmentError> {
    let entrypoint = nexa_entrypoint_surface(entrypoint, host_module, source, identity)?;
    Ok(RequiredEntrypointSurface {
        name: entrypoint.name,
        stable_id: entrypoint.stable_id,
        parameters: entrypoint.parameters,
        result: entrypoint.result,
        effect: entrypoint.effect,
        source: entrypoint.source,
    })
}

fn async_result_surface(
    contract: &ValidatedContract,
    function: &ValidatedFunction,
    host_module: &ModulePath,
) -> Result<HostAsyncResultSurface, PackageEnvironmentError> {
    let Some(result) = function.result.as_ref() else {
        return Err(PackageEnvironmentError::InvalidAsyncResult(
            function.name.clone(),
        ));
    };
    let ResolvedTypeKind::Result(success, error) = &result.kind else {
        return Err(PackageEnvironmentError::InvalidAsyncResult(
            function.name.clone(),
        ));
    };
    let success_value = nexa_contract::abi_value_type(success);
    let error_value = nexa_contract::abi_value_type(error);
    let cancel_error = match function.cancel_policy {
        CancelPolicy::ReturnError => Some(policy_error_tag(
            contract,
            error,
            &function.name,
            "Cancelled",
            u32::MAX - 1,
        )?),
        CancelPolicy::CancelTask => None,
    };
    let abandon_error = match function.abandon_policy {
        AbandonPolicy::ReturnError => Some(policy_error_tag(
            contract,
            error,
            &function.name,
            "Abandoned",
            u32::MAX,
        )?),
        AbandonPolicy::Trap => None,
    };
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
        cancel_error,
        abandon_error,
    })
}

fn policy_error_tag(
    contract: &ValidatedContract,
    error: &ResolvedTypeRef,
    function: &str,
    variant: &'static str,
    integer_fallback: u32,
) -> Result<u32, PackageEnvironmentError> {
    match &error.kind {
        ResolvedTypeKind::I32 => Ok(integer_fallback),
        ResolvedTypeKind::Named(named) => contract
            .enums
            .iter()
            .find(|enumeration| enumeration.stable_id == named.stable_id)
            .and_then(|enumeration| {
                enumeration
                    .variants
                    .iter()
                    .position(|candidate| candidate.name == variant && candidate.payload.is_none())
            })
            .and_then(|index| u32::try_from(index).ok())
            .ok_or_else(|| PackageEnvironmentError::MissingPolicyErrorVariant {
                function: function.to_owned(),
                variant,
            }),
        _ => Err(PackageEnvironmentError::MissingPolicyErrorVariant {
            function: function.to_owned(),
            variant,
        }),
    }
}

fn surface_type(
    ty: &ResolvedTypeRef,
    named_module: &ModulePath,
) -> Result<SurfaceType, PackageEnvironmentError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => SurfaceType::I32,
        ResolvedTypeKind::I64 => SurfaceType::I64,
        ResolvedTypeKind::F32 => SurfaceType::F32,
        ResolvedTypeKind::F64 => SurfaceType::F64,
        ResolvedTypeKind::Bool => SurfaceType::Bool,
        ResolvedTypeKind::Rune => SurfaceType::Rune,
        ResolvedTypeKind::String => SurfaceType::String,
        ResolvedTypeKind::Array(inner) => {
            SurfaceType::Array(Box::new(surface_type(inner, named_module)?))
        }
        ResolvedTypeKind::Buffer(inner) => {
            SurfaceType::Buffer(Box::new(surface_type(inner, named_module)?))
        }
        ResolvedTypeKind::Option(inner) => {
            SurfaceType::Option(Box::new(surface_type(inner, named_module)?))
        }
        ResolvedTypeKind::Result(success, error) => SurfaceType::Result(
            Box::new(surface_type(success, named_module)?),
            Box::new(surface_type(error, named_module)?),
        ),
        ResolvedTypeKind::Token(named) => SurfaceType::Token(Box::new(SurfaceType::Named {
            module: named_module.clone(),
            name: named.source_name.clone(),
        })),
        ResolvedTypeKind::Snapshot(named) => SurfaceType::Snapshot(Box::new(SurfaceType::Named {
            module: named_module.clone(),
            name: named.source_name.clone(),
        })),
        ResolvedTypeKind::Named(named) => SurfaceType::Named {
            module: named_module.clone(),
            name: named.source_name.clone(),
        },
    })
}

fn declaration_origin(
    source: &Arc<str>,
    identity: &SourceIdentity,
    span: SourceSpan,
) -> ExternalSourceOrigin {
    ExternalSourceOrigin {
        identity: identity.clone(),
        text: Arc::clone(source),
        range: ByteRange::new(span.start, span.end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: &str = r#"
contract Host;

handle Entity;

struct Pair {
    left: i32,
    right: i32,
}

enum Failure {
    Cancelled,
    Abandoned,
}

host {
    fn ping(value: Pair) -> i32;

    @fuel(7)
    @cancel(return_error)
    @abandon(return_error)
    @capability("profile.read")
    async fn load(entity: Entity) -> Result<Pair, Failure>;
}

nexa {
    fn on_event(value: Pair) -> bool;
    fn reset();
}
"#;

    #[test]
    fn v2_surface_contains_all_entrypoints_and_only_selected_requirements() {
        let contract = nexa_contract::parse_contract(CONTRACT).unwrap();
        let input = HostContractInput::with_source(
            &contract,
            SourceIdentity::standalone("memory://host.nidl"),
            CONTRACT,
        )
        .unwrap()
        .requiring_entrypoints(&["on_event".to_owned()])
        .unwrap();
        let surface = canonical_host_surface(&input).unwrap();

        assert_eq!(surface.functions.len(), 2);
        assert_eq!(surface.nexa_entrypoints.len(), 2);
        assert_eq!(surface.required_entrypoints.len(), 1);
        assert_eq!(surface.required_entrypoints[0].name, "on_event");
        assert_eq!(surface.functions[1].mode, HostFunctionMode::Request);
        assert!(surface.functions[1].async_result.is_some());
        assert_eq!(
            surface.functions[1].required_capabilities.as_slice(),
            ["profile.read"]
        );
    }

    #[test]
    fn exact_source_identity_and_spans_survive_the_adapter() {
        let contract = nexa_contract::parse_contract(CONTRACT).unwrap();
        let identity = SourceIdentity::standalone("memory://contracts/host.nidl");
        let input = HostContractInput::with_source(&contract, identity.clone(), CONTRACT).unwrap();
        let surface = canonical_host_surface(&input).unwrap();
        let source = surface.source.as_ref().unwrap();

        assert_eq!(source.identity, identity);
        assert_eq!(
            source.text.get(
                usize::try_from(source.range.start).unwrap()
                    ..usize::try_from(source.range.end).unwrap()
            ),
            Some(CONTRACT.trim())
        );
        assert!(surface.functions.iter().all(|function| {
            function
                .source
                .as_ref()
                .is_some_and(|origin| origin.identity == identity)
        }));
    }

    #[test]
    fn contract_entrypoints_are_optional_until_the_host_requires_them() {
        let contract = nexa_contract::parse_contract(CONTRACT).unwrap();
        let input = HostContractInput::canonical(&contract);
        assert_eq!(input.entrypoints().count(), 2);
        assert_eq!(input.required_entrypoints().count(), 0);
        assert!(
            canonical_host_surface(&input)
                .unwrap()
                .required_entrypoints
                .is_empty()
        );
    }
}
