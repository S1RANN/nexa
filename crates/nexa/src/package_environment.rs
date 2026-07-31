//! Canonical adapters from the exact Host IDL and compiler-provided standard library into the
//! shared semantic-analysis environment.
//!
//! This module performs no source-language parsing. Package and embedded standard-library sources
//! are parsed only by `nexa-analysis`; the adapters here preserve already-validated descriptor
//! identity and ABI metadata.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, ExternalFieldSurface, ExternalSourceOrigin, ExternalTypeKind,
    ExternalTypeSurface, ExternalVariantSurface, HostAsyncResultSurface, HostContractSurface,
    HostFunctionMode, HostFunctionSurface, IrAbandonPolicy, IrCancelPolicy, ModulePath,
    RequiredExportSurface, SurfaceType,
};
use nexa_bytecode::{ValueType, array_type, buffer_type, option_type, result_type, snapshot_type};
use nexa_core::StableId;
use nexa_diagnostics::{ByteRange, SourceIdentity};
use nexa_idl::{AbandonPolicy, CancelPolicy, Idl, TypeRef};

use crate::package_build::HostContractInput;

/// Stable reader-facing identity of the canonical Host contract snapshot retained in artifacts.
pub const CANONICAL_HOST_SOURCE_PATH: &str = "host-contract.nidl";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageEnvironmentError {
    TooManyHostFunctions,
    UntypedSnapshot,
    InvalidRequestResult(String),
    MissingPolicyErrorVariant {
        function: String,
        variant: &'static str,
    },
    MissingHostSourceLocation {
        kind: &'static str,
        name: String,
    },
    InvalidHostSource(nexa_idl::IdlError),
    HostSourceContractMismatch,
}

impl fmt::Display for PackageEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyHostFunctions => formatter.write_str("too many Host functions"),
            Self::UntypedSnapshot => {
                formatter.write_str("Host snapshots must carry a content type")
            }
            Self::InvalidRequestResult(function) => write!(
                formatter,
                "request Host function {function} must return request<Result<Success, Error>>"
            ),
            Self::MissingPolicyErrorVariant { function, variant } => write!(
                formatter,
                "request Host function {function} requires a zero-payload {variant} error variant"
            ),
            Self::MissingHostSourceLocation { kind, name } => {
                write!(
                    formatter,
                    "cannot locate {kind} `{name}` in the exact Host source"
                )
            }
            Self::InvalidHostSource(error) => write!(formatter, "invalid Host source: {error}"),
            Self::HostSourceContractMismatch => {
                formatter.write_str("Host source map does not match the supplied IDL")
            }
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

/// Builds the non-empty canonical analysis environment used by every product, test, CLI, Engine,
/// and LSP package pipeline.
///
/// Embedded-source modules and their compiler intrinsics are materialized directly by
/// `nexa-analysis` from the frozen standard-library descriptor. This adapter supplies the exact
/// Host ABI. No production caller is permitted to substitute `AnalysisEnvironment::default()`.
pub(crate) fn canonical_analysis_environment(
    contract: &HostContractInput<'_>,
) -> Result<AnalysisEnvironment, PackageEnvironmentError> {
    Ok(AnalysisEnvironment {
        host: Some(canonical_host_surface(contract)?),
        ..AnalysisEnvironment::default()
    })
}

/// Converts one exact, already-validated IDL into the sole Host semantic/codegen surface.
#[allow(clippy::too_many_lines)]
pub(crate) fn canonical_host_surface(
    contract: &HostContractInput<'_>,
) -> Result<HostContractSurface, PackageEnvironmentError> {
    let idl = contract.idl();
    let source = Arc::clone(contract.source().text());
    let identity = contract.source().identity().clone();
    let (parsed, source_map) = nexa_idl::parse_with_source_map(&source)
        .map_err(PackageEnvironmentError::InvalidHostSource)?;
    if parsed != *idl {
        return Err(PackageEnvironmentError::HostSourceContractMismatch);
    }
    let interface_origin = declaration_origin(&source, &identity, source_map.interface);
    let host_module = ModulePath::new("host").expect("host is a reserved valid module path");

    let mut types = Vec::new();
    for name in &idl.opaque_handles {
        types.push(ExternalTypeSurface {
            name: name.clone(),
            kind: ExternalTypeKind::Opaque,
            stable_id: Some(StableId::from_name(name)),
            type_parameters: Vec::new(),
            fields: Vec::new(),
            variants: Vec::new(),
            source: Some(declaration_origin(
                &source,
                &identity,
                named_source(&source_map.types, name, "opaque")?,
            )),
        });
    }
    for structure in &idl.structs {
        let type_origin = declaration_origin(
            &source,
            &identity,
            named_source(&source_map.types, &structure.name, "struct")?,
        );
        let fields = structure
            .fields
            .iter()
            .map(|field| {
                Ok(ExternalFieldSurface {
                    name: field.name.clone(),
                    stable_id: Some(StableId::from_parts(&[&structure.name, "::", &field.name])),
                    ty: surface_type(&field.ty, &host_module)?,
                    source: Some(declaration_origin(
                        &source,
                        &identity,
                        nested_source(
                            &source_map.fields,
                            &structure.name,
                            &field.name,
                            "Host struct field",
                        )?,
                    )),
                })
            })
            .collect::<Result<Vec<_>, PackageEnvironmentError>>()?;
        types.push(ExternalTypeSurface {
            name: structure.name.clone(),
            kind: ExternalTypeKind::Struct,
            stable_id: Some(StableId::from_name(&structure.name)),
            type_parameters: Vec::new(),
            fields,
            variants: Vec::new(),
            source: Some(type_origin),
        });
    }
    for enumeration in &idl.enums {
        let type_origin = declaration_origin(
            &source,
            &identity,
            named_source(&source_map.types, &enumeration.name, "enum")?,
        );
        let variants = enumeration
            .variants
            .iter()
            .map(|variant| {
                let payload = variant
                    .payload
                    .as_ref()
                    .map(|ty| surface_type(ty, &host_module))
                    .transpose()?
                    .into_iter()
                    .collect();
                Ok(ExternalVariantSurface {
                    name: variant.name.clone(),
                    stable_id: Some(StableId::from_parts(&[
                        &enumeration.name,
                        "::",
                        &variant.name,
                    ])),
                    payload,
                    source: Some(declaration_origin(
                        &source,
                        &identity,
                        nested_source(
                            &source_map.variants,
                            &enumeration.name,
                            &variant.name,
                            "Host enum variant",
                        )?,
                    )),
                })
            })
            .collect::<Result<Vec<_>, PackageEnvironmentError>>()?;
        types.push(ExternalTypeSurface {
            name: enumeration.name.clone(),
            kind: ExternalTypeKind::Enum,
            stable_id: Some(StableId::from_name(&enumeration.name)),
            type_parameters: Vec::new(),
            fields: Vec::new(),
            variants,
            source: Some(type_origin),
        });
    }

    let mut functions = Vec::with_capacity(idl.functions.len());
    for (index, function) in idl.functions.iter().enumerate() {
        let import_index =
            u32::try_from(index).map_err(|_| PackageEnvironmentError::TooManyHostFunctions)?;
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
                Some(request_result_surface(idl, function, &host_module)?),
            )
        };
        functions.push(HostFunctionSurface {
            name: function.name.clone(),
            parameters,
            result,
            mode,
            stable_id: StableId::from_parts(&[&idl.interface, "::", &function.name]),
            import_index,
            fuel_cost: function.fuel_cost,
            async_result,
            required_capability: None,
            source: Some(declaration_origin(
                &source,
                &identity,
                named_source(&source_map.functions, &function.name, "fn")?,
            )),
        });
    }

    let required_exports = contract
        .required_exports()
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
                source: Some(declaration_origin(
                    &source,
                    &identity,
                    named_source(&source_map.exports, &export.name, "export")?,
                )),
            })
        })
        .collect::<Result<Vec<_>, PackageEnvironmentError>>()?;

    Ok(HostContractSurface {
        interface_name: idl.interface.clone(),
        interface_stable_id: nexa_idl::exact_hash(idl),
        types,
        functions,
        required_exports,
        source: Some(interface_origin),
    })
}

fn request_result_surface(
    idl: &Idl,
    function: &nexa_idl::HostFunction,
    host_module: &ModulePath,
) -> Result<HostAsyncResultSurface, PackageEnvironmentError> {
    let TypeRef::HostRequest(Some(request_result)) = &function.result else {
        return Err(PackageEnvironmentError::InvalidRequestResult(
            function.name.clone(),
        ));
    };
    let TypeRef::Result(success, error) = request_result.as_ref() else {
        return Err(PackageEnvironmentError::InvalidRequestResult(
            function.name.clone(),
        ));
    };
    let success_value = idl_value_type(success)?;
    let error_value = idl_value_type(error)?;
    let result_type = result_type(success_value, error_value).type_id;
    let cancel_error = match function.cancel_policy {
        CancelPolicy::ReturnError => Some(policy_error_tag(
            idl,
            error,
            &function.name,
            "Cancelled",
            u32::MAX - 1,
        )?),
        CancelPolicy::CancelTask => None,
    };
    let abandon_error = match function.abandon_policy {
        AbandonPolicy::ReturnError => Some(policy_error_tag(
            idl,
            error,
            &function.name,
            "Abandoned",
            u32::MAX,
        )?),
        AbandonPolicy::Trap => None,
    };
    Ok(HostAsyncResultSurface {
        result_type,
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
    idl: &Idl,
    error: &TypeRef,
    function: &str,
    variant: &'static str,
    integer_fallback: u32,
) -> Result<u32, PackageEnvironmentError> {
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
                .ok_or_else(|| PackageEnvironmentError::MissingPolicyErrorVariant {
                    function: function.to_owned(),
                    variant,
                })
        }
        _ => Err(PackageEnvironmentError::MissingPolicyErrorVariant {
            function: function.to_owned(),
            variant,
        }),
    }
}

fn surface_type(
    ty: &TypeRef,
    named_module: &ModulePath,
) -> Result<SurfaceType, PackageEnvironmentError> {
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
        TypeRef::Snapshot(None) => return Err(PackageEnvironmentError::UntypedSnapshot),
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

fn idl_value_type(ty: &TypeRef) -> Result<ValueType, PackageEnvironmentError> {
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
            let ValueType::Named(content_type) = idl_value_type(inner)? else {
                return Err(PackageEnvironmentError::UntypedSnapshot);
            };
            ValueType::Named(snapshot_type(content_type))
        }
        TypeRef::Snapshot(None) => return Err(PackageEnvironmentError::UntypedSnapshot),
        TypeRef::Array(inner) => ValueType::Named(array_type(idl_value_type(inner)?)),
        TypeRef::Buffer(inner) => ValueType::Named(buffer_type(idl_value_type(inner)?)),
        TypeRef::Option(inner) => ValueType::Named(option_type(idl_value_type(inner)?).type_id),
        TypeRef::Result(success, error) => {
            ValueType::Named(result_type(idl_value_type(success)?, idl_value_type(error)?).type_id)
        }
        TypeRef::Named(name) => ValueType::Named(StableId::from_name(name)),
    })
}

fn declaration_origin(
    source: &Arc<str>,
    identity: &SourceIdentity,
    location: nexa_idl::IdlDeclarationSource,
) -> ExternalSourceOrigin {
    ExternalSourceOrigin {
        identity: identity.clone(),
        text: Arc::clone(source),
        range: ByteRange::new(
            u32_len(location.declaration_start),
            u32_len(location.declaration_end),
        ),
    }
}

fn named_source(
    locations: &BTreeMap<String, nexa_idl::IdlDeclarationSource>,
    name: &str,
    kind: &'static str,
) -> Result<nexa_idl::IdlDeclarationSource, PackageEnvironmentError> {
    locations
        .get(name)
        .copied()
        .ok_or_else(|| PackageEnvironmentError::MissingHostSourceLocation {
            kind,
            name: name.to_owned(),
        })
}

fn nested_source(
    locations: &BTreeMap<(String, String), nexa_idl::IdlDeclarationSource>,
    owner: &str,
    name: &str,
    kind: &'static str,
) -> Result<nexa_idl::IdlDeclarationSource, PackageEnvironmentError> {
    locations
        .get(&(owner.to_owned(), name.to_owned()))
        .copied()
        .ok_or_else(|| PackageEnvironmentError::MissingHostSourceLocation {
            kind,
            name: format!("{owner}::{name}"),
        })
}

fn u32_len(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_surface_preserves_exact_abi_and_source_origin() {
        let idl = nexa_idl::parse(
            r"
interface Host {
    enum Failure { Cancelled, Abandoned, Other }
    request(return_error, return_error) fuel 7 fn fail(value: i32)
        -> request<Result<string, Failure>>;
    export OnEvent(value: i32) -> bool;
}
",
        )
        .unwrap();
        let contract = HostContractInput::canonical(&idl);
        let surface = canonical_host_surface(&contract).unwrap();
        assert_eq!(surface.interface_stable_id, nexa_idl::exact_hash(&idl));
        assert_eq!(surface.functions[0].import_index, 0);
        assert_eq!(surface.functions[0].fuel_cost, 7);
        assert_eq!(
            surface.required_exports[0].stable_id,
            nexa_idl::export_stable_id(&idl, &idl.exports[0])
        );
        let origin = surface.functions[0].source.as_ref().unwrap();
        assert_eq!(origin.identity.path(), CANONICAL_HOST_SOURCE_PATH);
        let range = usize::try_from(origin.range.start).unwrap()
            ..usize::try_from(origin.range.end).unwrap();
        assert!(origin.text[range].contains("fn fail("));
        let async_result = surface.functions[0].async_result.as_ref().unwrap();
        assert_eq!(async_result.cancel_error, Some(0));
        assert_eq!(async_result.abandon_error, Some(1));
    }

    #[test]
    fn host_surface_retains_real_source_and_explicit_empty_nominal_kinds() {
        let source: Arc<str> = Arc::from(
            "interface Odd {\r\n\
             opaque   Token  ;\r\n\
             struct Empty\r\n{\r\n}\r\n\
             enum Nothing\r\n{\r\n}\r\n\
             }\r\n",
        );
        let idl = nexa_idl::parse(&source).unwrap();
        let identity = SourceIdentity::standalone("contracts/odd api.nidl");
        let contract =
            HostContractInput::with_source(&idl, identity.clone(), Arc::clone(&source)).unwrap();
        let surface = canonical_host_surface(&contract).unwrap();
        assert_eq!(surface.types[0].kind, ExternalTypeKind::Opaque);
        assert_eq!(surface.types[1].kind, ExternalTypeKind::Struct);
        assert_eq!(surface.types[2].kind, ExternalTypeKind::Enum);
        assert!(surface.types[1].fields.is_empty());
        assert!(surface.types[2].variants.is_empty());
        for ty in &surface.types {
            let origin = ty.source.as_ref().unwrap();
            assert_eq!(origin.identity, identity);
            assert_eq!(origin.text, source);
            assert!(origin.range.start < origin.range.end);
        }
    }

    #[test]
    fn required_export_subset_preserves_the_complete_host_surface() {
        let source: Arc<str> = Arc::from(
            "interface Host {\n\
             sync fn ping() -> i32;\n\
             export Run() -> i32;\n\
             export Reset() -> void;\n\
             }\n",
        );
        let idl = nexa_idl::parse(&source).unwrap();
        let identity = SourceIdentity::standalone("contracts/host.nidl");
        let full =
            HostContractInput::with_source(&idl, identity.clone(), Arc::clone(&source)).unwrap();
        let selected = full.requiring_exports(&["Run".to_owned()]).unwrap();
        let full_surface = canonical_host_surface(&full).unwrap();
        let selected_surface = canonical_host_surface(&selected).unwrap();

        assert_eq!(
            selected_surface.interface_stable_id,
            full_surface.interface_stable_id
        );
        assert_eq!(selected_surface.functions, full_surface.functions);
        assert_eq!(selected_surface.types, full_surface.types);
        assert_eq!(selected_surface.source, full_surface.source);
        assert_eq!(selected_surface.required_exports.len(), 1);
        assert_eq!(selected_surface.required_exports[0].name, "Run");
        assert_eq!(full_surface.required_exports.len(), 2);
    }

    #[test]
    fn host_surface_uses_parser_owned_unicode_and_collision_safe_ranges() {
        fn source_text(origin: &ExternalSourceOrigin) -> &str {
            &origin.text[usize::try_from(origin.range.start).unwrap()
                ..usize::try_from(origin.range.end).unwrap()]
        }

        let source: Arc<str> = Arc::from(
            " \r\ninterface Høst {\r\n\
             opaque Second;\r\n\
             struct Pair { first: Second; Second: i32; }\r\n\
             enum Résult { Ok(Second), Second }\r\n\
             sync fuel 9 fn løad(value: Pair) -> Résult;\r\n\
             export Rün(value: Pair) -> i32;\r\n\
             }\r\n ",
        );
        let idl = nexa_idl::parse(&source).unwrap();
        let identity = SourceIdentity::standalone("contracts/Høst api.nidl");
        let contract =
            HostContractInput::with_source(&idl, identity.clone(), Arc::clone(&source)).unwrap();
        let surface = canonical_host_surface(&contract).unwrap();

        let interface = surface.source.as_ref().unwrap();
        assert_eq!(interface.identity, identity);
        assert!(source_text(interface).starts_with("interface Høst"));
        assert!(source_text(interface).ends_with('}'));
        assert!(!source_text(interface).starts_with(' '));

        let pair = surface.types.iter().find(|ty| ty.name == "Pair").unwrap();
        assert_eq!(
            source_text(pair.fields[0].source.as_ref().unwrap()),
            "first: Second;"
        );
        assert_eq!(
            source_text(pair.fields[1].source.as_ref().unwrap()),
            "Second: i32;"
        );
        let result = surface.types.iter().find(|ty| ty.name == "Résult").unwrap();
        assert_eq!(
            source_text(result.variants[1].source.as_ref().unwrap()),
            "Second"
        );
        assert_eq!(
            source_text(surface.functions[0].source.as_ref().unwrap()),
            "sync fuel 9 fn løad(value: Pair) -> Résult;"
        );
        assert_eq!(
            source_text(surface.required_exports[0].source.as_ref().unwrap()),
            "export Rün(value: Pair) -> i32;"
        );
    }
}
