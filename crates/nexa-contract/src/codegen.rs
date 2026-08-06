//! Structured Rust binding generation for validated Contracts.
//!
//! The backend deliberately operates on [`BindingModel`] instead of syntax or AST nodes. Every
//! Rust fragment is constructed as a [`TokenStream`], parsed as a complete [`syn::File`], formatted
//! by `prettyplease`, and parsed again before it is returned to a build script.

#![allow(clippy::needless_pass_by_value, clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nexa_bytecode::ValueType;
use nexa_core::{SourceSpan, StableId};
use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote};

use crate::descriptor::{
    ABI_DESCRIPTOR_VERSION, EffectiveContractSelection, CONTRACT_SYNTAX_VERSION, abi_descriptor,
    effective_contract_fingerprint,
};
use crate::model::{
    AbandonPolicy, CancelPolicy, FunctionRustNames, NamedAbiKind, ResolvedTypeKind,
    ResolvedTypeRef, RustName, ValidatedContract, ValidatedEnum, ValidatedFunction,
    ValidatedStruct,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecStrategy {
    Unit,
    Scalar,
    String,
    Handle,
    Token,
    Snapshot,
    Array,
    Buffer,
    Option,
    Result,
    Struct,
    Enum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BorrowStrategy {
    Owned,
    BorrowedString,
    BorrowedCollection,
    BorrowedAggregate,
}

#[derive(Clone, Debug)]
pub struct BindingIdentity {
    pub source_name: String,
    pub rust_ident: Ident,
    pub rust_name: RustName,
    pub rust_type_name: String,
    pub stable_id: StableId,
    pub source_origin: SourceSpan,
}

#[derive(Clone, Debug)]
pub struct BindingTypeRef {
    pub source: ResolvedTypeRef,
    pub abi_type: ValueType,
    pub codec_strategy: CodecStrategy,
    pub borrow_strategy: BorrowStrategy,
    pub source_origin: SourceSpan,
}

#[derive(Clone, Debug)]
pub struct BindingHandle {
    pub identity: BindingIdentity,
    pub token_wrapper_ident: Option<Ident>,
    pub declaration_fingerprint: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct BindingField {
    pub identity: BindingIdentity,
    pub ty: BindingTypeRef,
}

#[derive(Clone, Debug)]
pub struct BindingVariant {
    pub identity: BindingIdentity,
    pub tag: u32,
    pub payload: Option<BindingTypeRef>,
}

#[derive(Clone, Debug)]
pub struct BindingStruct {
    pub identity: BindingIdentity,
    pub borrowed_ref_ident: Ident,
    pub fields: Vec<BindingField>,
    pub snapshot_names: Option<BindingSnapshotNames>,
    pub snapshot_schema_id: StableId,
    pub declaration_fingerprint: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct BindingSnapshotNames {
    pub wrapper_ident: Ident,
    pub encoder_ident: Ident,
    pub borrowed_ref_ident: Ident,
}

#[derive(Clone, Debug)]
pub struct BindingEnum {
    pub identity: BindingIdentity,
    pub borrowed_ref_ident: Ident,
    pub variants: Vec<BindingVariant>,
    pub declaration_fingerprint: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct BindingParameter {
    pub identity: BindingIdentity,
    pub ty: BindingTypeRef,
}

#[derive(Clone, Debug)]
pub struct BindingFunction {
    pub identity: BindingIdentity,
    pub marker_ident: Ident,
    pub args_ident: Ident,
    pub output_ident: Ident,
    pub completion_ticket_ident: Option<Ident>,
    pub is_async: bool,
    pub parameters: Vec<BindingParameter>,
    pub result: Option<BindingTypeRef>,
    pub fuel_cost: u32,
    pub cancel_policy: CancelPolicy,
    pub abandon_policy: AbandonPolicy,
    pub capabilities: Vec<String>,
    pub declaration_fingerprint: [u8; 32],
    pub host_contract: Option<BindingHostFunctionContract>,
}

#[derive(Clone, Debug)]
pub struct BindingHostFunctionContract {
    pub parameters: Vec<ValueType>,
    pub result: Option<ValueType>,
    pub mode: nexa_bytecode::HostCallMode,
    pub async_result: Option<nexa_bytecode::AsyncResultType>,
}

#[derive(Clone, Debug)]
pub struct BindingModel {
    pub identity: BindingIdentity,
    pub host_trait_ident: Ident,
    pub fingerprint: [u8; 32],
    pub contract_runtime_id: StableId,
    pub canonical_descriptor: Vec<u8>,
    pub source_text: String,
    pub handles: Vec<BindingHandle>,
    pub structs: Vec<BindingStruct>,
    pub enums: Vec<BindingEnum>,
    /// Canonical thunk order. Source declaration order never reaches generated dispatch IDs.
    pub host_functions: Vec<BindingFunction>,
    pub nexa_functions: Vec<BindingFunction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodegenError {
    InvalidRustIdentifier {
        name: String,
        span: SourceSpan,
    },
    RustNameCollision {
        name: String,
        first: SourceSpan,
        second: SourceSpan,
    },
    UnknownNamedType {
        name: String,
        span: SourceSpan,
    },
    InvalidTypedHandle {
        kind: &'static str,
        span: SourceSpan,
    },
    UnsupportedSnapshotContent {
        kind: &'static str,
        span: SourceSpan,
    },
    InvalidAsyncHostContract {
        name: String,
        span: SourceSpan,
    },
    TooManyFields {
        name: String,
        count: usize,
        span: SourceSpan,
    },
    TooManyParameters {
        name: String,
        count: usize,
        span: SourceSpan,
    },
    MissingDeclarationFingerprint {
        name: String,
        span: SourceSpan,
    },
    EffectiveTypeFingerprint {
        name: String,
        message: String,
        span: SourceSpan,
    },
    GeneratedSyntax(String),
    FormattedSyntax(String),
}

impl fmt::Display for CodegenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRustIdentifier { name, span } => write!(
                formatter,
                "`{name}` cannot be emitted as a Rust identifier at bytes {}..{}",
                span.start, span.end
            ),
            Self::RustNameCollision {
                name,
                first,
                second,
            } => write!(
                formatter,
                "generated Rust name `{name}` collides at bytes {}..{} and {}..{}",
                first.start, first.end, second.start, second.end
            ),
            Self::UnknownNamedType { name, span } => write!(
                formatter,
                "unknown Contract type `{name}` at bytes {}..{}",
                span.start, span.end
            ),
            Self::InvalidTypedHandle { kind, span } => write!(
                formatter,
                "{kind} requires a nominal type at bytes {}..{}",
                span.start, span.end
            ),
            Self::UnsupportedSnapshotContent { kind, span } => write!(
                formatter,
                "{kind} cannot be embedded in a generated snapshot codec at bytes {}..{}",
                span.start, span.end
            ),
            Self::InvalidAsyncHostContract { name, span } => write!(
                formatter,
                "validated async Host function `{name}` has inconsistent result policy at bytes \
                 {}..{}",
                span.start, span.end
            ),
            Self::TooManyFields { name, count, span } => write!(
                formatter,
                "`{name}` has {count} fields, exceeding the generated runtime codec at bytes \
                 {}..{}",
                span.start, span.end
            ),
            Self::TooManyParameters { name, count, span } => write!(
                formatter,
                "`{name}` has {count} parameters, exceeding the runtime limit at bytes {}..{}",
                span.start, span.end
            ),
            Self::MissingDeclarationFingerprint { name, span } => write!(
                formatter,
                "ABI descriptor omitted `{name}` at bytes {}..{}",
                span.start, span.end
            ),
            Self::EffectiveTypeFingerprint {
                name,
                message,
                span,
            } => write!(
                formatter,
                "cannot fingerprint recursive layout for `{name}` at bytes {}..{}: {message}",
                span.start, span.end
            ),
            Self::GeneratedSyntax(error) => {
                write!(formatter, "generated Rust tokens are invalid: {error}")
            }
            Self::FormattedSyntax(error) => {
                write!(formatter, "formatted generated Rust is invalid: {error}")
            }
        }
    }
}

impl std::error::Error for CodegenError {}

impl BindingModel {
    pub fn from_contract(contract: &ValidatedContract) -> Result<Self, CodegenError> {
        validate_snapshot_contents(contract)?;

        let identity = binding_identity(
            &contract.name,
            contract.stable_id,
            contract.name_span,
            &contract.rust_names.contract,
        );
        let host_trait_ident = validated_ident(&contract.rust_names.host_trait);
        let descriptor = abi_descriptor(contract);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let snapshot_schemas =
            contract
                .structs
                .iter()
                .map(|structure| {
                    let mut selection = EffectiveContractSelection::default();
                    selection.referenced_types.insert(structure.name.clone());
                    let fingerprint = effective_contract_fingerprint(contract, &selection)
                        .map_err(|error| CodegenError::EffectiveTypeFingerprint {
                            name: structure.name.clone(),
                            message: error.to_string(),
                            span: structure.span,
                        })?;
                    let schema_id = StableId(u64::from_le_bytes(
                        fingerprint.0[..8]
                            .try_into()
                            .expect("effective fingerprints have eight leading bytes"),
                    ));
                    Ok((structure.stable_id, schema_id))
                })
                .collect::<Result<BTreeMap<_, _>, CodegenError>>()?;
        let contract_runtime_id = StableId(u64::from_le_bytes(
            fingerprint[..8]
                .try_into()
                .expect("the ABI fingerprint always has eight leading bytes"),
        ));

        let mut handles = contract
            .handles
            .iter()
            .map(|handle| BindingHandle {
                identity: binding_identity(
                    &handle.name,
                    handle.stable_id,
                    handle.name_span,
                    &handle.rust_names.owned,
                ),
                token_wrapper_ident: handle
                    .rust_names
                    .token_wrapper
                    .as_ref()
                    .map(validated_ident),
                declaration_fingerprint: handle.declaration_fingerprint.into_bytes(),
            })
            .collect::<Vec<_>>();
        let mut structs = contract
            .structs
            .iter()
            .map(|structure| {
                let snapshot_schema_id = snapshot_schemas
                    .get(&structure.stable_id)
                    .copied()
                    .ok_or_else(|| CodegenError::EffectiveTypeFingerprint {
                        name: structure.name.clone(),
                        message: "descriptor omitted the selected type".into(),
                        span: structure.span,
                    })?;
                binding_struct(structure, snapshot_schema_id)
            })
            .collect::<Result<Vec<_>, CodegenError>>()?;
        let mut enums = contract
            .enums
            .iter()
            .map(binding_enum)
            .collect::<Result<Vec<_>, CodegenError>>()?;
        let mut host_functions = contract
            .host_functions
            .iter()
            .map(|function| binding_function(contract, function, true))
            .collect::<Result<Vec<_>, CodegenError>>()?;
        let mut nexa_functions = contract
            .nexa_functions
            .iter()
            .map(|function| binding_function(contract, function, false))
            .collect::<Result<Vec<_>, CodegenError>>()?;
        host_functions
            .sort_by(|left, right| canonical_identity_order(&left.identity, &right.identity));
        nexa_functions
            .sort_by(|left, right| canonical_identity_order(&left.identity, &right.identity));
        handles.sort_by(|left, right| canonical_identity_order(&left.identity, &right.identity));
        structs.sort_by(|left, right| canonical_identity_order(&left.identity, &right.identity));
        enums.sort_by(|left, right| canonical_identity_order(&left.identity, &right.identity));
        Ok(Self {
            identity,
            host_trait_ident,
            fingerprint,
            contract_runtime_id,
            canonical_descriptor: descriptor.bytes,
            source_text: contract.source.clone(),
            handles,
            structs,
            enums,
            host_functions,
            nexa_functions,
        })
    }

    fn structure(&self, stable_id: StableId) -> Option<&BindingStruct> {
        self.structs
            .iter()
            .find(|item| item.identity.stable_id == stable_id)
    }

    fn enumeration(&self, stable_id: StableId) -> Option<&BindingEnum> {
        self.enums
            .iter()
            .find(|item| item.identity.stable_id == stable_id)
    }

    fn handle(&self, stable_id: StableId) -> Option<&BindingHandle> {
        self.handles
            .iter()
            .find(|item| item.identity.stable_id == stable_id)
    }
}

fn validate_snapshot_contents(contract: &ValidatedContract) -> Result<(), CodegenError> {
    fn collect_snapshots(ty: &ResolvedTypeRef, output: &mut BTreeSet<StableId>) {
        match &ty.kind {
            ResolvedTypeKind::Snapshot(target) => {
                output.insert(target.stable_id);
            }
            ResolvedTypeKind::Array(inner)
            | ResolvedTypeKind::Buffer(inner)
            | ResolvedTypeKind::Option(inner) => collect_snapshots(inner, output),
            ResolvedTypeKind::Result(success, error) => {
                collect_snapshots(success, output);
                collect_snapshots(error, output);
            }
            ResolvedTypeKind::I32
            | ResolvedTypeKind::I64
            | ResolvedTypeKind::F32
            | ResolvedTypeKind::F64
            | ResolvedTypeKind::Bool
            | ResolvedTypeKind::Rune
            | ResolvedTypeKind::String
            | ResolvedTypeKind::Token(_)
            | ResolvedTypeKind::Named(_) => {}
        }
    }

    fn validate_value(
        contract: &ValidatedContract,
        ty: &ResolvedTypeRef,
        active: &mut BTreeSet<StableId>,
    ) -> Result<(), CodegenError> {
        match &ty.kind {
            ResolvedTypeKind::Token(_) => Err(CodegenError::UnsupportedSnapshotContent {
                kind: "Token",
                span: ty.span,
            }),
            ResolvedTypeKind::Snapshot(_) => Err(CodegenError::UnsupportedSnapshotContent {
                kind: "Snapshot",
                span: ty.span,
            }),
            ResolvedTypeKind::Array(inner)
            | ResolvedTypeKind::Buffer(inner)
            | ResolvedTypeKind::Option(inner) => validate_value(contract, inner, active),
            ResolvedTypeKind::Result(success, error) => {
                validate_value(contract, success, active)?;
                validate_value(contract, error, active)
            }
            ResolvedTypeKind::Named(named) => match named.kind {
                NamedAbiKind::Handle => Err(CodegenError::UnsupportedSnapshotContent {
                    kind: "handle",
                    span: ty.span,
                }),
                NamedAbiKind::Struct => {
                    if !active.insert(named.stable_id) {
                        return Ok(());
                    }
                    let structure = contract
                        .structs
                        .iter()
                        .find(|structure| structure.stable_id == named.stable_id)
                        .expect("resolved struct identity is present in its Contract");
                    for field in &structure.fields {
                        validate_value(contract, &field.ty, active)?;
                    }
                    active.remove(&named.stable_id);
                    Ok(())
                }
                NamedAbiKind::Enum => {
                    if !active.insert(named.stable_id) {
                        return Ok(());
                    }
                    let enumeration = contract
                        .enums
                        .iter()
                        .find(|enumeration| enumeration.stable_id == named.stable_id)
                        .expect("resolved enum identity is present in its Contract");
                    for payload in enumeration
                        .variants
                        .iter()
                        .filter_map(|variant| variant.payload.as_ref())
                    {
                        validate_value(contract, payload, active)?;
                    }
                    active.remove(&named.stable_id);
                    Ok(())
                }
            },
            ResolvedTypeKind::I32
            | ResolvedTypeKind::I64
            | ResolvedTypeKind::F32
            | ResolvedTypeKind::F64
            | ResolvedTypeKind::Bool
            | ResolvedTypeKind::Rune
            | ResolvedTypeKind::String => Ok(()),
        }
    }

    let mut snapshot_contents = BTreeSet::new();
    for ty in contract
        .structs
        .iter()
        .flat_map(|structure| structure.fields.iter().map(|field| &field.ty))
        .chain(contract.enums.iter().flat_map(|enumeration| {
            enumeration
                .variants
                .iter()
                .filter_map(|variant| variant.payload.as_ref())
        }))
        .chain(
            contract
                .host_functions
                .iter()
                .chain(contract.nexa_functions.iter())
                .flat_map(|function| {
                    function
                        .parameters
                        .iter()
                        .map(|parameter| &parameter.ty)
                        .chain(function.result.iter())
                }),
        )
    {
        collect_snapshots(ty, &mut snapshot_contents);
    }
    for content in snapshot_contents {
        if let Some(structure) = contract
            .structs
            .iter()
            .find(|structure| structure.stable_id == content)
        {
            for field in &structure.fields {
                validate_value(contract, &field.ty, &mut BTreeSet::new())?;
            }
        }
    }
    Ok(())
}

fn binding_identity(
    source_name: &str,
    stable_id: StableId,
    source_origin: SourceSpan,
    rust_name: &RustName,
) -> BindingIdentity {
    BindingIdentity {
        source_name: source_name.to_owned(),
        rust_ident: validated_ident(rust_name),
        rust_name: rust_name.clone(),
        rust_type_name: rust_name.as_str().to_owned(),
        stable_id,
        source_origin,
    }
}

fn validated_ident(name: &RustName) -> Ident {
    Ident::new(name.as_str(), Span::call_site())
}

fn canonical_identity_order(left: &BindingIdentity, right: &BindingIdentity) -> std::cmp::Ordering {
    left.source_name
        .as_bytes()
        .cmp(right.source_name.as_bytes())
        .then_with(|| left.stable_id.cmp(&right.stable_id))
}

fn binding_struct(
    structure: &ValidatedStruct,
    snapshot_schema_id: StableId,
) -> Result<BindingStruct, CodegenError> {
    if structure.fields.len() > nexa_bytecode::MAX_STRUCT_FIELDS {
        return Err(CodegenError::TooManyFields {
            name: structure.name.clone(),
            count: structure.fields.len(),
            span: structure.span,
        });
    }
    Ok(BindingStruct {
        identity: binding_identity(
            &structure.name,
            structure.stable_id,
            structure.name_span,
            &structure.rust_names.owned,
        ),
        borrowed_ref_ident: validated_ident(&structure.rust_names.borrowed_ref),
        fields: structure
            .fields
            .iter()
            .map(|field| {
                Ok(BindingField {
                    identity: binding_identity(
                        &field.name,
                        field.stable_id,
                        field.name_span,
                        &field.rust_name,
                    ),
                    ty: binding_type_ref(&field.ty)?,
                })
            })
            .collect::<Result<Vec<_>, CodegenError>>()?,
        snapshot_names: structure
            .rust_names
            .snapshot
            .as_ref()
            .map(|names| BindingSnapshotNames {
                wrapper_ident: validated_ident(&names.wrapper),
                encoder_ident: validated_ident(&names.encoder),
                borrowed_ref_ident: validated_ident(&names.borrowed_ref),
            }),
        snapshot_schema_id,
        declaration_fingerprint: structure.declaration_fingerprint.into_bytes(),
    })
}

fn binding_enum(enumeration: &ValidatedEnum) -> Result<BindingEnum, CodegenError> {
    Ok(BindingEnum {
        identity: binding_identity(
            &enumeration.name,
            enumeration.stable_id,
            enumeration.name_span,
            &enumeration.rust_names.owned,
        ),
        borrowed_ref_ident: validated_ident(&enumeration.rust_names.borrowed_ref),
        variants: enumeration
            .variants
            .iter()
            .enumerate()
            .map(|(tag, variant)| {
                Ok(BindingVariant {
                    identity: binding_identity(
                        &variant.name,
                        variant.stable_id,
                        variant.name_span,
                        &variant.rust_name,
                    ),
                    tag: u32::try_from(tag).map_err(|_| CodegenError::TooManyFields {
                        name: enumeration.name.clone(),
                        count: enumeration.variants.len(),
                        span: enumeration.span,
                    })?,
                    payload: variant.payload.as_ref().map(binding_type_ref).transpose()?,
                })
            })
            .collect::<Result<Vec<_>, CodegenError>>()?,
        declaration_fingerprint: enumeration.declaration_fingerprint.into_bytes(),
    })
}

fn binding_function(
    contract: &ValidatedContract,
    function: &ValidatedFunction,
    host: bool,
) -> Result<BindingFunction, CodegenError> {
    if function.parameters.len() > 8 {
        return Err(CodegenError::TooManyParameters {
            name: function.name.clone(),
            count: function.parameters.len(),
            span: function.span,
        });
    }
    let host_contract = host
        .then(|| binding_host_function_contract(contract, function))
        .transpose()?;
    let (identity_name, marker_ident, args_ident, output_ident, completion_ticket_ident) =
        match (&function.rust_names, host) {
            (
                FunctionRustNames::Host {
                    method,
                    completion_ticket,
                },
                true,
            ) => {
                let method_ident = validated_ident(method);
                (
                    method,
                    method_ident.clone(),
                    method_ident.clone(),
                    method_ident,
                    completion_ticket.as_ref().map(validated_ident),
                )
            }
            (
                FunctionRustNames::Nexa {
                    marker,
                    args,
                    output,
                },
                false,
            ) => (
                marker,
                validated_ident(marker),
                validated_ident(args),
                validated_ident(output),
                None,
            ),
            _ => {
                return Err(CodegenError::GeneratedSyntax(format!(
                    "validated Rust-name role does not match function `{}`",
                    function.name
                )));
            }
        };
    Ok(BindingFunction {
        identity: binding_identity(
            &function.name,
            function.stable_id,
            function.name_span,
            identity_name,
        ),
        marker_ident,
        args_ident,
        output_ident,
        completion_ticket_ident,
        is_async: function.is_async,
        parameters: function
            .parameters
            .iter()
            .map(|parameter| {
                Ok(BindingParameter {
                    identity: binding_identity(
                        &parameter.name,
                        parameter.stable_id,
                        parameter.name_span,
                        &parameter.rust_name,
                    ),
                    ty: binding_type_ref(&parameter.ty)?,
                })
            })
            .collect::<Result<Vec<_>, CodegenError>>()?,
        result: function.result.as_ref().map(binding_type_ref).transpose()?,
        fuel_cost: function.fuel_cost,
        cancel_policy: function.cancel_policy,
        abandon_policy: function.abandon_policy,
        capabilities: function.capabilities.clone(),
        declaration_fingerprint: function.declaration_fingerprint.into_bytes(),
        host_contract,
    })
}

fn binding_host_function_contract(
    contract: &ValidatedContract,
    function: &ValidatedFunction,
) -> Result<BindingHostFunctionContract, CodegenError> {
    let parameters = function
        .parameters
        .iter()
        .map(|parameter| parameter.ty.value_type())
        .collect::<Vec<_>>();
    let declared_result = function.result.as_ref().map(ResolvedTypeRef::value_type);
    if !function.is_async {
        return Ok(BindingHostFunctionContract {
            parameters,
            result: declared_result,
            mode: nexa_bytecode::HostCallMode::Immediate,
            async_result: None,
        });
    }

    let Some(result) = function.result.as_ref() else {
        return Err(CodegenError::InvalidAsyncHostContract {
            name: function.name.clone(),
            span: function.name_span,
        });
    };
    let ResolvedTypeKind::Result(success, error) = &result.kind else {
        return Err(CodegenError::InvalidAsyncHostContract {
            name: function.name.clone(),
            span: result.span,
        });
    };
    let success = success.value_type();
    let error_type = error.value_type();
    let result_type = nexa_bytecode::result_type(success, error_type).type_id;
    if declared_result != Some(ValueType::Named(result_type)) {
        return Err(CodegenError::InvalidAsyncHostContract {
            name: function.name.clone(),
            span: result.span,
        });
    }
    let cancel_error = match function.cancel_policy {
        CancelPolicy::ReturnError => Some(binding_policy_error_tag(
            contract,
            error,
            "Cancelled",
            u32::MAX - 1,
            function,
        )?),
        CancelPolicy::CancelTask => None,
    };
    let abandon_error = match function.abandon_policy {
        AbandonPolicy::ReturnError => Some(binding_policy_error_tag(
            contract,
            error,
            "Abandoned",
            u32::MAX,
            function,
        )?),
        AbandonPolicy::Trap => None,
    };
    Ok(BindingHostFunctionContract {
        parameters,
        result: Some(ValueType::Named(result_type)),
        mode: nexa_bytecode::HostCallMode::Async,
        async_result: Some(nexa_bytecode::AsyncResultType {
            result_type,
            success,
            error: error_type,
            cancel_policy: match function.cancel_policy {
                CancelPolicy::ReturnError => nexa_bytecode::CancelPolicy::ReturnError,
                CancelPolicy::CancelTask => nexa_bytecode::CancelPolicy::CancelTask,
            },
            abandon_policy: match function.abandon_policy {
                AbandonPolicy::ReturnError => nexa_bytecode::AbandonPolicy::ReturnError,
                AbandonPolicy::Trap => nexa_bytecode::AbandonPolicy::Trap,
            },
            cancel_error,
            abandon_error,
        }),
    })
}

fn binding_policy_error_tag(
    contract: &ValidatedContract,
    error: &ResolvedTypeRef,
    variant: &str,
    integer_fallback: u32,
    function: &ValidatedFunction,
) -> Result<u32, CodegenError> {
    match &error.kind {
        ResolvedTypeKind::I32 => Ok(integer_fallback),
        ResolvedTypeKind::Named(named) if named.kind == NamedAbiKind::Enum => contract
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
            .ok_or_else(|| CodegenError::InvalidAsyncHostContract {
                name: function.name.clone(),
                span: error.span,
            }),
        _ => Err(CodegenError::InvalidAsyncHostContract {
            name: function.name.clone(),
            span: error.span,
        }),
    }
}

fn binding_type_ref(ty: &ResolvedTypeRef) -> Result<BindingTypeRef, CodegenError> {
    let abi_type = ty.value_type();
    let (codec_strategy, borrow_strategy) = match &ty.kind {
        ResolvedTypeKind::I32
        | ResolvedTypeKind::I64
        | ResolvedTypeKind::F32
        | ResolvedTypeKind::F64
        | ResolvedTypeKind::Bool
        | ResolvedTypeKind::Rune => (CodecStrategy::Scalar, BorrowStrategy::Owned),
        ResolvedTypeKind::String => (CodecStrategy::String, BorrowStrategy::BorrowedString),
        ResolvedTypeKind::Token(_) => (CodecStrategy::Token, BorrowStrategy::Owned),
        ResolvedTypeKind::Snapshot(_) => (CodecStrategy::Snapshot, BorrowStrategy::Owned),
        ResolvedTypeKind::Array(_) => (CodecStrategy::Array, BorrowStrategy::BorrowedCollection),
        ResolvedTypeKind::Buffer(_) => (CodecStrategy::Buffer, BorrowStrategy::BorrowedCollection),
        ResolvedTypeKind::Option(inner) => (
            CodecStrategy::Option,
            binding_type_ref(inner)?.borrow_strategy,
        ),
        ResolvedTypeKind::Result(success, error) => {
            let success = binding_type_ref(success)?.borrow_strategy;
            let error = binding_type_ref(error)?.borrow_strategy;
            (
                CodecStrategy::Result,
                if success == BorrowStrategy::Owned && error == BorrowStrategy::Owned {
                    BorrowStrategy::Owned
                } else {
                    BorrowStrategy::BorrowedAggregate
                },
            )
        }
        ResolvedTypeKind::Named(named) => match named.kind {
            NamedAbiKind::Handle => (CodecStrategy::Handle, BorrowStrategy::Owned),
            NamedAbiKind::Struct => (CodecStrategy::Struct, BorrowStrategy::BorrowedAggregate),
            NamedAbiKind::Enum => (CodecStrategy::Enum, BorrowStrategy::BorrowedAggregate),
        },
    };
    Ok(BindingTypeRef {
        source: ty.clone(),
        abi_type,
        codec_strategy,
        borrow_strategy,
        source_origin: ty.span,
    })
}

pub fn generate_rust_tokens(contract: &ValidatedContract) -> Result<TokenStream, CodegenError> {
    let model = BindingModel::from_contract(contract)?;
    generate_model_tokens(&model)
}

pub fn generate_rust(contract: &ValidatedContract) -> Result<String, CodegenError> {
    let tokens = generate_rust_tokens(contract)?;
    let file = syn::parse2::<syn::File>(tokens)
        .map_err(|error| CodegenError::GeneratedSyntax(error.to_string()))?;
    let formatted = prettyplease::unparse(&file);
    let mut generated = String::with_capacity(
        "// @generated by nexa-idl v2. DO NOT EDIT.\n".len() + formatted.len(),
    );
    generated.push_str("// @generated by nexa-idl v2. DO NOT EDIT.\n");
    generated.push_str(&formatted);
    syn::parse_file(&generated)
        .map_err(|error| CodegenError::FormattedSyntax(error.to_string()))?;
    Ok(generated)
}

fn generate_model_tokens(model: &BindingModel) -> Result<TokenStream, CodegenError> {
    let header = generate_contract_header(model);
    let types = generate_types(model)?;
    let handles = generate_handles(model)?;
    let host = generate_host_surface(model)?;
    let nexa = generate_nexa_surface(model)?;
    Ok(quote! {
        #header
        #types
        #handles
        #host
        #nexa
    })
}

fn generate_contract_header(model: &BindingModel) -> TokenStream {
    let fingerprint = model.fingerprint.iter();
    let descriptor = model.canonical_descriptor.iter();
    let contract_runtime_id = model.contract_runtime_id.0;
    let contract_name = &model.identity.source_name;
    let source_text = &model.source_text;
    let contract_syntax_version = CONTRACT_SYNTAX_VERSION;
    let abi_descriptor_version = ABI_DESCRIPTOR_VERSION;
    quote! {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct HostError(pub ::std::string::String);

        impl ::std::fmt::Display for HostError {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl ::std::error::Error for HostError {}

        #[doc(hidden)]
        pub struct __NexaArrayRef<'a, T> {
            raw: nexa_runtime::HostArrayRef<'a>,
            decode: fn(
                nexa_runtime::HostValueRef<'a>,
            ) -> ::std::result::Result<T, nexa_runtime::HostTrap>,
        }

        impl<'a, T> ::std::clone::Clone for __NexaArrayRef<'a, T> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<'a, T> ::std::marker::Copy for __NexaArrayRef<'a, T> {}

        impl<'a, T: 'a> __NexaArrayRef<'a, T> {
            fn __nexa_from_runtime(
                raw: nexa_runtime::HostArrayRef<'a>,
                decode: fn(
                    nexa_runtime::HostValueRef<'a>,
                ) -> ::std::result::Result<T, nexa_runtime::HostTrap>,
            ) -> Self {
                Self { raw, decode }
            }

            pub const fn len(self) -> usize {
                self.raw.len()
            }

            pub const fn is_empty(self) -> bool {
                self.raw.is_empty()
            }

            pub fn get(
                self,
                index: usize,
            ) -> ::std::result::Result<T, nexa_runtime::HostTrap> {
                (self.decode)(self.raw.get(index)?)
            }

            pub fn iter(
                self,
            ) -> impl ::std::iter::ExactSizeIterator<
                Item = ::std::result::Result<T, nexa_runtime::HostTrap>,
            > + 'a {
                let decode = self.decode;
                self.raw.iter().map(decode)
            }
        }

        #[doc(hidden)]
        pub struct __NexaBufferRef<'a, T> {
            raw: nexa_runtime::HostBufferRef<'a>,
            decode: fn(
                nexa_runtime::HostValueRef<'a>,
            ) -> ::std::result::Result<T, nexa_runtime::HostTrap>,
        }

        impl<'a, T> ::std::clone::Clone for __NexaBufferRef<'a, T> {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<'a, T> ::std::marker::Copy for __NexaBufferRef<'a, T> {}

        impl<'a, T: 'a> __NexaBufferRef<'a, T> {
            fn __nexa_from_runtime(
                raw: nexa_runtime::HostBufferRef<'a>,
                decode: fn(
                    nexa_runtime::HostValueRef<'a>,
                ) -> ::std::result::Result<T, nexa_runtime::HostTrap>,
            ) -> Self {
                Self { raw, decode }
            }

            pub const fn len(self) -> usize {
                self.raw.len()
            }

            pub const fn is_empty(self) -> bool {
                self.raw.is_empty()
            }

            pub fn get(
                self,
                index: usize,
            ) -> ::std::result::Result<T, nexa_runtime::HostTrap> {
                (self.decode)(self.raw.get(index)?)
            }

            pub fn iter(
                self,
            ) -> impl ::std::iter::ExactSizeIterator<
                Item = ::std::result::Result<T, nexa_runtime::HostTrap>,
            > + 'a {
                let decode = self.decode;
                self.raw.iter().map(decode)
            }
        }

        pub const CONTRACT_SOURCE_NAME: &str = #contract_name;
        pub const CONTRACT_SYNTAX_VERSION: u16 = #contract_syntax_version;
        pub const HOST_CONTRACT_SCHEMA_VERSION: u32 = 2;
        pub const ABI_DESCRIPTOR_VERSION: u16 = #abi_descriptor_version;
        pub const CONTRACT_FINGERPRINT: [u8; 32] = [#(#fingerprint),*];
        pub const CONTRACT_RUNTIME_ID: nexa_runtime::StableId =
            nexa_runtime::StableId(#contract_runtime_id);
        pub const CONTRACT_DESCRIPTOR: &[u8] = &[#(#descriptor),*];
        pub const SOURCE: &str = #source_text;

        pub const fn contract() -> nexa_runtime::HostContract {
            nexa_runtime::HostContract::new(
                CONTRACT_SOURCE_NAME,
                SOURCE,
                CONTRACT_DESCRIPTOR,
                CONTRACT_FINGERPRINT,
                CONTRACT_RUNTIME_ID,
                HOST_CONTRACT_SCHEMA_VERSION,
            )
        }
    }
}

fn generate_handles(model: &BindingModel) -> Result<TokenStream, CodegenError> {
    let handles = model.handles.iter().map(|handle| {
        let ident = &handle.identity.rust_ident;
        quote! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
            pub struct #ident(pub u64);
        }
    });
    let tokens = model.handles.iter().filter_map(|handle| {
        let ident = handle.token_wrapper_ident.as_ref()?;
        let content_id = handle.identity.stable_id.0;
        let token_id = nexa_bytecode::resource_token_type(handle.identity.stable_id).0;
        Some(quote! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
            pub struct #ident(nexa_runtime::ResourceTokenHandle);

            impl #ident {
                pub const CONTENT_TYPE_ID: nexa_runtime::StableId =
                    nexa_runtime::StableId(#content_id);
                pub const TOKEN_TYPE_ID: nexa_runtime::StableId =
                    nexa_runtime::StableId(#token_id);

                pub fn try_from_raw(
                    handle: nexa_runtime::ResourceTokenHandle,
                ) -> ::std::result::Result<Self, nexa_runtime::HostTrap> {
                    if handle.content_type() == Self::CONTENT_TYPE_ID {
                        ::std::result::Result::Ok(Self(handle))
                    } else {
                        ::std::result::Result::Err(nexa_runtime::HostTrap::Type)
                    }
                }

                pub const fn into_raw(self) -> nexa_runtime::ResourceTokenHandle {
                    self.0
                }
            }

            impl ::std::convert::TryFrom<nexa_runtime::ResourceTokenHandle> for #ident {
                type Error = nexa_runtime::HostTrap;

                fn try_from(
                    value: nexa_runtime::ResourceTokenHandle,
                ) -> ::std::result::Result<Self, Self::Error> {
                    Self::try_from_raw(value)
                }
            }

            impl ::std::convert::From<#ident> for nexa_runtime::ResourceTokenHandle {
                fn from(value: #ident) -> Self {
                    value.into_raw()
                }
            }
        })
    });
    let snapshots = model
        .structs
        .iter()
        .filter(|structure| structure.snapshot_names.is_some())
        .map(|structure| generate_snapshot_handle(model, structure))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    Ok(quote! {
        #(#handles)*
        #(#tokens)*
        #(#snapshots)*
    })
}

fn generate_snapshot_handle(
    model: &BindingModel,
    structure: &BindingStruct,
) -> Result<TokenStream, CodegenError> {
    let content_id = structure.identity.stable_id;
    let snapshot_id = nexa_bytecode::snapshot_type(content_id);
    let content_id_value = content_id.0;
    let snapshot_id_value = snapshot_id.0;
    let names = structure
        .snapshot_names
        .as_ref()
        .expect("snapshot generation is filtered by resolved wrapper names");
    let snapshot_ident = &names.wrapper_ident;
    let common = quote! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct #snapshot_ident(nexa_runtime::SnapshotHandle);

        impl #snapshot_ident {
            pub const TYPE_ID: nexa_runtime::StableId =
                nexa_runtime::StableId(#snapshot_id_value);

            pub fn try_from_raw(
                handle: nexa_runtime::SnapshotHandle,
            ) -> ::std::result::Result<Self, nexa_runtime::HostTrap> {
                if handle.type_id() == Self::TYPE_ID {
                    ::std::result::Result::Ok(Self(handle))
                } else {
                    ::std::result::Result::Err(nexa_runtime::HostTrap::Type)
                }
            }

            pub const fn into_raw(self) -> nexa_runtime::SnapshotHandle {
                self.0
            }
        }

        impl ::std::convert::TryFrom<nexa_runtime::SnapshotHandle> for #snapshot_ident {
            type Error = nexa_runtime::HostTrap;

            fn try_from(
                value: nexa_runtime::SnapshotHandle,
            ) -> ::std::result::Result<Self, Self::Error> {
                Self::try_from_raw(value)
            }
        }

        impl ::std::convert::From<#snapshot_ident> for nexa_runtime::SnapshotHandle {
            fn from(value: #snapshot_ident) -> Self {
                value.into_raw()
            }
        }
    };
    let content_ident = &structure.identity.rust_ident;
    let encoder_ident = &names.encoder_ident;
    let ref_ident = &names.borrowed_ref_ident;
    let schema_id = structure.snapshot_schema_id;
    let schema_id_value = schema_id.0;
    let encode_fields = structure
        .fields
        .iter()
        .map(|field| {
            let field_ident = &field.identity.rust_ident;
            snapshot_encode(
                model,
                &field.ty.source,
                quote!(value.#field_ident),
                quote!(__nexa_bytes),
                false,
            )
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let decode_fields = structure
        .fields
        .iter()
        .map(|field| {
            let field_ident = &field.identity.rust_ident;
            let value = snapshot_decode(
                model,
                &field.ty.source,
                quote!(__nexa_payload),
                quote!(__nexa_cursor),
            )?;
            Ok(quote!(#field_ident: #value))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    Ok(quote! {
        #common

        pub struct #encoder_ident;

        impl #encoder_ident {
            pub const CONTENT_TYPE: nexa_runtime::StableId =
                nexa_runtime::StableId(#content_id_value);
            pub const SCHEMA_HASH: nexa_runtime::StableId =
                nexa_runtime::StableId(#schema_id_value);

            pub fn encode(
                value: &#content_ident,
            ) -> Result<nexa_runtime::EncodedSnapshot, HostError> {
                let mut __nexa_bytes = ::std::vec::Vec::new();
                #(#encode_fields)*
                nexa_runtime::EncodedSnapshot::new(
                    Self::CONTENT_TYPE,
                    Self::SCHEMA_HASH,
                    1,
                    ::std::sync::Arc::from(__nexa_bytes),
                )
                .map_err(|_| HostError("failed to encode snapshot".into()))
            }
        }

        #[derive(Clone, Copy, Debug)]
        pub struct #ref_ident<'a>(nexa_runtime::TypedSnapshotRef<'a>);

        impl<'a> nexa_runtime::DecodeTypedSnapshot<'a> for #ref_ident<'a> {
            const TYPE_ID: nexa_runtime::StableId =
                nexa_runtime::StableId(#snapshot_id_value);
            const CONTENT_TYPE: nexa_runtime::StableId =
                nexa_runtime::StableId(#content_id_value);
            const SCHEMA_HASH: nexa_runtime::StableId =
                nexa_runtime::StableId(#schema_id_value);
            const ALIGNMENT: u16 = 1;

            fn decode(
                view: nexa_runtime::TypedSnapshotRef<'a>,
            ) -> ::std::result::Result<Self, nexa_runtime::HostTrap> {
                ::std::result::Result::Ok(Self(view))
            }
        }

        impl #ref_ident<'_> {
            pub fn decode_owned(
                self,
            ) -> ::std::result::Result<#content_ident, nexa_runtime::HostTrap> {
                let __nexa_payload = self.0.payload();
                let mut __nexa_cursor = 0usize;
                let value = #content_ident { #(#decode_fields),* };
                if __nexa_cursor != __nexa_payload.len() {
                    return ::std::result::Result::Err(nexa_runtime::HostTrap::Type);
                }
                ::std::result::Result::Ok(value)
            }
        }
    })
}

fn generate_types(model: &BindingModel) -> Result<TokenStream, CodegenError> {
    let structures = model
        .structs
        .iter()
        .map(|structure| generate_struct(model, structure))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let enums = model
        .enums
        .iter()
        .map(|enumeration| generate_enum(model, enumeration))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    Ok(quote! {
        #(#structures)*
        #(#enums)*
    })
}

fn generate_struct(
    model: &BindingModel,
    structure: &BindingStruct,
) -> Result<TokenStream, CodegenError> {
    let ident = &structure.identity.rust_ident;
    let ref_ident = &structure.borrowed_ref_ident;
    let type_id = structure.identity.stable_id.0;
    let fields = structure
        .fields
        .iter()
        .map(|field| {
            let field_ident = &field.identity.rust_ident;
            let ty = owned_rust_type(model, &field.ty.source)?;
            Ok(quote!(pub #field_ident: #ty))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let accessors = structure
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let field_ident = &field.identity.rust_ident;
            let ty = input_rust_type(model, &field.ty.source, quote!('a))?;
            let decoded =
                decode_host_value_ref(model, &field.ty.source, quote!(self.0.field(#index)?))?;
            Ok(quote! {
                #[allow(clippy::needless_question_mark)]
                pub fn #field_ident(self) -> Result<#ty, nexa_runtime::HostTrap> {
                    ::std::result::Result::Ok(#decoded)
                }
            })
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let requirements = requirements_for_struct(model, structure, quote!(self))?;
    let encoded_fields = structure
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let field_ident = &field.identity.rust_ident;
            let encoded = encode_runtime_value(
                model,
                &field.ty.source,
                quote!(self.#field_ident),
                quote!(transaction),
            )?;
            Ok(quote!(__nexa_fields[#index] = #encoded;))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let completion_fields = structure
        .fields
        .iter()
        .map(|field| {
            let field_ident = &field.identity.rust_ident;
            encode_completion_payload(model, &field.ty.source, quote!(self.#field_ident))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let script_fields = structure
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let field_ident = &field.identity.rust_ident;
            let value = decode_script_output(
                model,
                &field.ty.source,
                quote!(
                    __nexa_struct
                        .field(#index)
                        .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
                ),
            )?;
            Ok(quote!(#field_ident: #value))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let snapshot_encode_fields = structure
        .fields
        .iter()
        .map(|field| {
            let field_ident = &field.identity.rust_ident;
            snapshot_encode(
                model,
                &field.ty.source,
                quote!(self.#field_ident),
                quote!(__nexa_bytes),
                false,
            )
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let snapshot_decode_fields = structure
        .fields
        .iter()
        .map(|field| {
            let field_ident = &field.identity.rust_ident;
            let value = snapshot_decode(
                model,
                &field.ty.source,
                quote!(__nexa_payload),
                quote!(*__nexa_cursor),
            )?;
            Ok(quote!(#field_ident: #value))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let field_count = structure.fields.len();
    Ok(quote! {
        #[derive(Clone, Debug, PartialEq)]
        pub struct #ident {
            #(#fields),*
        }

        #[allow(dead_code)]
        impl #ident {
            fn __nexa_requirements(
                &self,
            ) -> Result<
                nexa_runtime::HostReturnRequirements,
                nexa_runtime::HostTrap,
            > {
                ::std::result::Result::Ok(#requirements)
            }

            fn __nexa_encode_runtime(
                self,
                transaction: &mut nexa_runtime::HostReturnTransaction<'_>,
            ) -> Result<nexa_runtime::RuntimeValue, nexa_runtime::HostTrap> {
                let mut __nexa_fields = [
                    nexa_runtime::RuntimeValue::Unit;
                    nexa_runtime::MAX_HOST_RETURN_FIELDS
                ];
                #(#encoded_fields)*
                transaction.write_struct(
                    nexa_runtime::StableId(#type_id),
                    &__nexa_fields[..#field_count],
                )
            }

            fn __nexa_completion_payload(self) -> nexa_runtime::HostPayload {
                nexa_runtime::HostPayload::structure([#(#completion_fields),*])
            }

            fn __nexa_decode_script(
                value: nexa_runtime::HostValueRef<'_>,
            ) -> Result<Self, nexa_runtime::ScriptCallError> {
                let __nexa_struct =
                    value
                        .struct_ref(nexa_runtime::StableId(#type_id))
                        .map_err(|_| {
                            nexa_runtime::ScriptCallError::OutputDecoding
                        })?;
                if __nexa_struct.len() != #field_count {
                    return ::std::result::Result::Err(
                        nexa_runtime::ScriptCallError::OutputDecoding
                    );
                }
                ::std::result::Result::Ok(Self { #(#script_fields),* })
            }

            fn __nexa_snapshot_encode(
                &self,
                __nexa_bytes: &mut ::std::vec::Vec<u8>,
            ) -> Result<(), HostError> {
                #(#snapshot_encode_fields)*
                ::std::result::Result::Ok(())
            }

            fn __nexa_snapshot_decode(
                __nexa_payload: &[u8],
                __nexa_cursor: &mut usize,
            ) -> Result<Self, nexa_runtime::HostTrap> {
                ::std::result::Result::Ok(Self { #(#snapshot_decode_fields),* })
            }
        }

        #[derive(Clone, Copy, Debug)]
        pub struct #ref_ident<'a>(nexa_runtime::HostStructRef<'a>);

        impl<'a> #ref_ident<'a> {
            fn __nexa_from_runtime(
                value: nexa_runtime::HostStructRef<'a>,
            ) -> Result<Self, nexa_runtime::HostTrap> {
                if value.len() != #field_count {
                    return ::std::result::Result::Err(nexa_runtime::HostTrap::Type);
                }
                ::std::result::Result::Ok(Self(value))
            }

            #(#accessors)*
        }

        impl nexa_runtime::EncodeHostReturn for #ident {
            fn requirements(
                &self,
            ) -> Result<nexa_runtime::HostReturnRequirements, nexa_runtime::HostTrap> {
                Self::__nexa_requirements(self)
            }

            fn encode_into(
                self,
                transaction: &mut nexa_runtime::HostReturnTransaction<'_>,
            ) -> Result<nexa_runtime::RuntimeValue, nexa_runtime::HostTrap> {
                Self::__nexa_encode_runtime(self, transaction)
            }
        }
    })
}

fn generate_enum(
    model: &BindingModel,
    enumeration: &BindingEnum,
) -> Result<TokenStream, CodegenError> {
    let ident = &enumeration.identity.rust_ident;
    let ref_ident = &enumeration.borrowed_ref_ident;
    let type_id = enumeration.identity.stable_id.0;
    let variants = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            if let Some(payload) = &variant.payload {
                let ty = owned_rust_type(model, &payload.source)?;
                Ok(quote!(#variant_ident(#ty)))
            } else {
                Ok(quote!(#variant_ident))
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let tag_arms = enumeration.variants.iter().map(|variant| {
        let variant_ident = &variant.identity.rust_ident;
        let tag = variant.tag;
        if variant.payload.is_some() {
            quote!(Self::#variant_ident(_) => #tag)
        } else {
            quote!(Self::#variant_ident => #tag)
        }
    });
    let ref_variants = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            if let Some(payload) = &variant.payload {
                let ty = input_rust_type(model, &payload.source, quote!('a))?;
                Ok(quote!(#variant_ident(#ty)))
            } else {
                Ok(quote!(#variant_ident))
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let decode_arms = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            let variant_id = variant.identity.stable_id.0;
            let tag = variant.tag;
            if let Some(payload) = &variant.payload {
                let decoded = decode_host_value_ref(
                    model,
                    &payload.source,
                    quote!(value.payload().ok_or(nexa_runtime::HostTrap::Type)?),
                )?;
                Ok(quote! {
                    (nexa_runtime::StableId(#variant_id), #tag) =>
                        ::std::result::Result::Ok(Self::#variant_ident(#decoded))
                })
            } else {
                Ok(quote! {
                    (nexa_runtime::StableId(#variant_id), #tag)
                        if value.payload().is_none() =>
                        ::std::result::Result::Ok(Self::#variant_ident)
                })
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let requirement_arms = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            if let Some(payload) = &variant.payload {
                let nested = requirements_for_type(model, &payload.source, quote!(value))?;
                Ok(quote! {
                    Self::#variant_ident(value) => {
                        let _ = &value;
                        #nested
                    }
                })
            } else {
                Ok(quote! {
                    Self::#variant_ident =>
                        nexa_runtime::HostReturnRequirements::ZERO
                })
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let requirements = quote! {
        nexa_runtime::HostReturnRequirements {
            object_slots: 1,
            ..nexa_runtime::HostReturnRequirements::ZERO
        }
        .checked_add(match self {
            #(#requirement_arms),*
        })
    };
    let runtime_arms = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            let variant_id = variant.identity.stable_id.0;
            let tag = variant.tag;
            if let Some(payload) = &variant.payload {
                let nested = encode_runtime_value(
                    model,
                    &payload.source,
                    quote!(value),
                    quote!(transaction),
                )?;
                Ok(quote! {
                    Self::#variant_ident(value) => {
                        let __nexa_payload = #nested;
                        transaction.write_enum(
                            nexa_runtime::StableId(#type_id),
                            nexa_runtime::StableId(#variant_id),
                            #tag,
                            ::std::option::Option::Some(__nexa_payload),
                        )?
                    }
                })
            } else {
                Ok(quote! {
                    Self::#variant_ident => transaction.write_enum(
                        nexa_runtime::StableId(#type_id),
                        nexa_runtime::StableId(#variant_id),
                        #tag,
                        ::std::option::Option::None,
                    )?
                })
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let completion_arms = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            let variant_id = variant.identity.stable_id.0;
            let tag = variant.tag;
            if let Some(payload) = &variant.payload {
                let nested = encode_completion_payload(model, &payload.source, quote!(value))?;
                Ok(quote! {
                    Self::#variant_ident(value) => nexa_runtime::HostPayload::Enum {
                        type_id: nexa_runtime::StableId(#type_id),
                        variant: nexa_runtime::StableId(#variant_id),
                        tag: #tag,
                        payload: ::std::option::Option::Some(
                            ::std::boxed::Box::new(#nested)
                        ),
                    }
                })
            } else {
                Ok(quote! {
                    Self::#variant_ident => nexa_runtime::HostPayload::Enum {
                        type_id: nexa_runtime::StableId(#type_id),
                        variant: nexa_runtime::StableId(#variant_id),
                        tag: #tag,
                        payload: ::std::option::Option::None,
                    }
                })
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let script_arms = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            let variant_id = variant.identity.stable_id.0;
            let tag = variant.tag;
            if let Some(payload) = &variant.payload {
                let nested = decode_script_output(
                    model,
                    &payload.source,
                    quote!(
                        __nexa_enum
                            .payload()
                            .ok_or(nexa_runtime::ScriptCallError::OutputDecoding)?
                    ),
                )?;
                Ok(quote! {
                    (nexa_runtime::StableId(#variant_id), #tag) =>
                        ::std::result::Result::Ok(Self::#variant_ident(#nested))
                })
            } else {
                Ok(quote! {
                    (nexa_runtime::StableId(#variant_id), #tag)
                        if __nexa_enum.payload().is_none() =>
                        ::std::result::Result::Ok(Self::#variant_ident)
                })
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let snapshot_encode_arms = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            let tag = variant.tag;
            if let Some(payload) = &variant.payload {
                let nested = snapshot_encode(
                    model,
                    &payload.source,
                    quote!(__nexa_payload),
                    quote!(__nexa_bytes),
                    true,
                )?;
                Ok(quote! {
                    Self::#variant_ident(__nexa_payload) => {
                        __nexa_bytes.extend_from_slice(&#tag.to_le_bytes());
                        #nested
                    }
                })
            } else {
                Ok(quote! {
                    Self::#variant_ident => {
                        __nexa_bytes.extend_from_slice(&#tag.to_le_bytes());
                    }
                })
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let snapshot_decode_arms = enumeration
        .variants
        .iter()
        .map(|variant| {
            let variant_ident = &variant.identity.rust_ident;
            let tag = variant.tag;
            if let Some(payload) = &variant.payload {
                let nested = snapshot_decode(
                    model,
                    &payload.source,
                    quote!(__nexa_payload),
                    quote!(*__nexa_cursor),
                )?;
                Ok(quote!(#tag => Self::#variant_ident(#nested)))
            } else {
                Ok(quote!(#tag => Self::#variant_ident))
            }
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    Ok(quote! {
        #[derive(Clone, Debug, PartialEq)]
        pub enum #ident {
            #(#variants),*
        }

        #[allow(dead_code)]
        impl #ident {
            pub const fn nexa_tag(&self) -> u32 {
                match self {
                    #(#tag_arms),*
                }
            }

            fn __nexa_requirements(
                &self,
            ) -> Result<
                nexa_runtime::HostReturnRequirements,
                nexa_runtime::HostTrap,
            > {
                #requirements
            }

            fn __nexa_encode_runtime(
                self,
                transaction: &mut nexa_runtime::HostReturnTransaction<'_>,
            ) -> Result<nexa_runtime::RuntimeValue, nexa_runtime::HostTrap> {
                ::std::result::Result::Ok(match self {
                    #(#runtime_arms),*
                })
            }

            fn __nexa_completion_payload(self) -> nexa_runtime::HostPayload {
                match self {
                    #(#completion_arms),*
                }
            }

            fn __nexa_decode_script(
                value: nexa_runtime::HostValueRef<'_>,
            ) -> Result<Self, nexa_runtime::ScriptCallError> {
                let __nexa_enum =
                    value
                        .enum_ref(nexa_runtime::StableId(#type_id))
                        .map_err(|_| {
                            nexa_runtime::ScriptCallError::OutputDecoding
                        })?;
                match (__nexa_enum.variant(), __nexa_enum.tag()) {
                    #(#script_arms),*,
                    _ => ::std::result::Result::Err(
                        nexa_runtime::ScriptCallError::OutputDecoding
                    ),
                }
            }

            fn __nexa_snapshot_encode(
                &self,
                __nexa_bytes: &mut ::std::vec::Vec<u8>,
            ) -> Result<(), HostError> {
                match self {
                    #(#snapshot_encode_arms),*
                }
                ::std::result::Result::Ok(())
            }

            fn __nexa_snapshot_decode(
                __nexa_payload: &[u8],
                __nexa_cursor: &mut usize,
            ) -> Result<Self, nexa_runtime::HostTrap> {
                let __nexa_end = (*__nexa_cursor)
                    .checked_add(4)
                    .ok_or(nexa_runtime::HostTrap::Type)?;
                let __nexa_tag = u32::from_le_bytes(
                    __nexa_payload
                        .get(*__nexa_cursor..__nexa_end)
                        .ok_or(nexa_runtime::HostTrap::Type)?
                        .try_into()
                        .map_err(|_| nexa_runtime::HostTrap::Type)?,
                );
                *__nexa_cursor = __nexa_end;
                ::std::result::Result::Ok(match __nexa_tag {
                    #(#snapshot_decode_arms),*,
                    _ => return ::std::result::Result::Err(
                        nexa_runtime::HostTrap::Type
                    ),
                })
            }
        }

        #[derive(Clone, Copy, Debug)]
        pub enum #ref_ident<'a> {
            #(#ref_variants),*,
            #[doc(hidden)]
            __Lifetime(::std::marker::PhantomData<&'a ()>),
        }

        impl<'a> #ref_ident<'a> {
            fn __nexa_from_runtime(
                value: nexa_runtime::HostEnumRef<'a>,
            ) -> Result<Self, nexa_runtime::HostTrap> {
                if value.type_id() != nexa_runtime::StableId(#type_id) {
                    return ::std::result::Result::Err(nexa_runtime::HostTrap::Type);
                }
                match (value.variant(), value.tag()) {
                    #(#decode_arms),*,
                    _ => ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
                }
            }
        }

        impl nexa_runtime::EncodeHostReturn for #ident {
            fn requirements(
                &self,
            ) -> Result<nexa_runtime::HostReturnRequirements, nexa_runtime::HostTrap> {
                Self::__nexa_requirements(self)
            }

            fn encode_into(
                self,
                transaction: &mut nexa_runtime::HostReturnTransaction<'_>,
            ) -> Result<nexa_runtime::RuntimeValue, nexa_runtime::HostTrap> {
                Self::__nexa_encode_runtime(self, transaction)
            }
        }
    })
}

fn owned_rust_type(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => quote!(i32),
        ResolvedTypeKind::I64 => quote!(i64),
        ResolvedTypeKind::F32 => quote!(f32),
        ResolvedTypeKind::F64 => quote!(f64),
        ResolvedTypeKind::Bool => quote!(bool),
        ResolvedTypeKind::Rune => quote!(char),
        ResolvedTypeKind::String => quote!(::std::string::String),
        ResolvedTypeKind::Array(inner) => {
            let inner = owned_rust_type(model, inner)?;
            quote!(::std::vec::Vec<#inner>)
        }
        ResolvedTypeKind::Buffer(inner) => {
            let inner = owned_rust_type(model, inner)?;
            quote!(nexa_runtime::CopyBuffer<#inner>)
        }
        ResolvedTypeKind::Option(inner) => {
            let inner = owned_rust_type(model, inner)?;
            quote!(::std::option::Option<#inner>)
        }
        ResolvedTypeKind::Result(success, error) => {
            let success = owned_rust_type(model, success)?;
            let error = owned_rust_type(model, error)?;
            quote!(::std::result::Result<#success, #error>)
        }
        ResolvedTypeKind::Token(target) => {
            let ident = model
                .handle(target.stable_id)
                .and_then(|handle| handle.token_wrapper_ident.as_ref())
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: target.source_name.clone(),
                    span: ty.span,
                })?;
            quote!(#ident)
        }
        ResolvedTypeKind::Snapshot(target) => {
            let ident = model
                .structure(target.stable_id)
                .and_then(|structure| structure.snapshot_names.as_ref())
                .map(|names| &names.wrapper_ident)
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: target.source_name.clone(),
                    span: ty.span,
                })?;
            quote!(#ident)
        }
        ResolvedTypeKind::Named(named) => {
            let ident = validated_ident(&named.rust_name);
            quote!(#ident)
        }
    })
}

fn input_rust_type(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    lifetime: TokenStream,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::String => quote!(&#lifetime str),
        ResolvedTypeKind::Array(inner) => {
            let inner = input_rust_type(model, inner, lifetime.clone())?;
            quote!(__NexaArrayRef<#lifetime, #inner>)
        }
        ResolvedTypeKind::Buffer(inner) => {
            let inner = input_rust_type(model, inner, lifetime.clone())?;
            quote!(__NexaBufferRef<#lifetime, #inner>)
        }
        ResolvedTypeKind::Option(inner) => {
            let inner = input_rust_type(model, inner, lifetime)?;
            quote!(::std::option::Option<#inner>)
        }
        ResolvedTypeKind::Result(success, error) => {
            let success = input_rust_type(model, success, lifetime.clone())?;
            let error = input_rust_type(model, error, lifetime)?;
            quote!(::std::result::Result<#success, #error>)
        }
        ResolvedTypeKind::Named(named) if named.kind == NamedAbiKind::Struct => {
            let ident = &model
                .structure(named.stable_id)
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: named.source_name.clone(),
                    span: ty.span,
                })?
                .borrowed_ref_ident;
            quote!(#ident<#lifetime>)
        }
        ResolvedTypeKind::Named(named) if named.kind == NamedAbiKind::Enum => {
            let ident = &model
                .enumeration(named.stable_id)
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: named.source_name.clone(),
                    span: ty.span,
                })?
                .borrowed_ref_ident;
            quote!(#ident<#lifetime>)
        }
        _ => owned_rust_type(model, ty)?,
    })
}

fn type_borrows(ty: &ResolvedTypeRef) -> bool {
    match &ty.kind {
        ResolvedTypeKind::String | ResolvedTypeKind::Array(_) | ResolvedTypeKind::Buffer(_) => true,
        ResolvedTypeKind::Option(inner) => type_borrows(inner),
        ResolvedTypeKind::Result(success, error) => type_borrows(success) || type_borrows(error),
        ResolvedTypeKind::Named(named) => named.kind != NamedAbiKind::Handle,
        ResolvedTypeKind::I32
        | ResolvedTypeKind::I64
        | ResolvedTypeKind::F32
        | ResolvedTypeKind::F64
        | ResolvedTypeKind::Bool
        | ResolvedTypeKind::Rune
        | ResolvedTypeKind::Token(_)
        | ResolvedTypeKind::Snapshot(_) => false,
    }
}

fn runtime_value_type(ty: &ResolvedTypeRef) -> TokenStream {
    let value = ty.value_type();
    match value {
        ValueType::I32 => quote!(nexa_runtime::ValueType::I32),
        ValueType::I64 => quote!(nexa_runtime::ValueType::I64),
        ValueType::F32 => quote!(nexa_runtime::ValueType::F32),
        ValueType::F64 => quote!(nexa_runtime::ValueType::F64),
        ValueType::Bool => quote!(nexa_runtime::ValueType::Bool),
        ValueType::Rune => quote!(nexa_runtime::ValueType::Rune),
        ValueType::String => quote!(nexa_runtime::ValueType::String),
        ValueType::Ref => quote!(nexa_runtime::ValueType::Ref),
        ValueType::Named(id) => {
            let id = id.0;
            quote!(nexa_runtime::ValueType::Named(nexa_runtime::StableId(#id)))
        }
    }
}

fn binding_value_type(ty: &ResolvedTypeRef) -> ValueType {
    ty.value_type()
}

fn requirements_source_for_place(ty: &ResolvedTypeRef, place: TokenStream) -> TokenStream {
    match &ty.kind {
        ResolvedTypeKind::Option(_) | ResolvedTypeKind::Result(_, _) => quote!(&(#place)),
        ResolvedTypeKind::Named(named)
            if matches!(named.kind, NamedAbiKind::Struct | NamedAbiKind::Enum) =>
        {
            quote!(&(#place))
        }
        _ => place,
    }
}

fn requirements_for_struct(
    model: &BindingModel,
    structure: &BindingStruct,
    source: TokenStream,
) -> Result<TokenStream, CodegenError> {
    let field_count = structure.fields.len();
    let additions = structure
        .fields
        .iter()
        .map(|field| {
            let field_ident = &field.identity.rust_ident;
            let requirement_source =
                requirements_source_for_place(&field.ty.source, quote!((#source).#field_ident));
            let requirement = requirements_for_type(model, &field.ty.source, requirement_source)?;
            Ok(quote! {
                __nexa_requirements =
                    __nexa_requirements.checked_add(#requirement)?;
            })
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    Ok(quote! {{
        let mut __nexa_requirements = nexa_runtime::HostReturnRequirements {
            object_slots: 1,
            struct_fields: #field_count,
            ..nexa_runtime::HostReturnRequirements::ZERO
        };
        #(#additions)*
        __nexa_requirements
    }})
}

fn requirements_for_type(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    source: TokenStream,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32
        | ResolvedTypeKind::I64
        | ResolvedTypeKind::F32
        | ResolvedTypeKind::F64
        | ResolvedTypeKind::Bool
        | ResolvedTypeKind::Rune
        | ResolvedTypeKind::Token(_)
        | ResolvedTypeKind::Snapshot(_) => quote!(nexa_runtime::HostReturnRequirements::ZERO),
        ResolvedTypeKind::String => quote! {
            nexa_runtime::HostReturnRequirements {
                object_slots: 1,
                string_bytes: (#source).len(),
                ..nexa_runtime::HostReturnRequirements::ZERO
            }
        },
        ResolvedTypeKind::Array(inner) => {
            let nested = requirements_for_type(model, inner, quote!(value))?;
            quote! {{
                let mut __nexa_requirements = nexa_runtime::HostReturnRequirements {
                    object_slots: 1,
                    collection_elements: (#source).len(),
                    ..nexa_runtime::HostReturnRequirements::ZERO
                };
                for value in (#source).iter() {
                    let _ = &value;
                    __nexa_requirements =
                        __nexa_requirements.checked_add(#nested)?;
                }
                __nexa_requirements
            }}
        }
        ResolvedTypeKind::Buffer(inner) => {
            let nested = requirements_for_type(model, inner, quote!(value))?;
            quote! {{
                let mut __nexa_requirements = nexa_runtime::HostReturnRequirements {
                    object_slots: 1,
                    collection_elements: (#source).len(),
                    ..nexa_runtime::HostReturnRequirements::ZERO
                };
                for value in (#source).as_slice().iter() {
                    let _ = &value;
                    __nexa_requirements =
                        __nexa_requirements.checked_add(#nested)?;
                }
                __nexa_requirements
            }}
        }
        ResolvedTypeKind::Option(inner) => {
            let nested = requirements_for_type(model, inner, quote!(value))?;
            quote! {
                nexa_runtime::HostReturnRequirements {
                    object_slots: 1,
                    ..nexa_runtime::HostReturnRequirements::ZERO
                }
                .checked_add(match #source {
                    ::std::option::Option::Some(value) => {
                        let _ = &value;
                        #nested
                    },
                    ::std::option::Option::None =>
                        nexa_runtime::HostReturnRequirements::ZERO,
                })?
            }
        }
        ResolvedTypeKind::Result(success, error) => {
            let success = requirements_for_type(model, success, quote!(value))?;
            let error = requirements_for_type(model, error, quote!(error))?;
            quote! {
                nexa_runtime::HostReturnRequirements {
                    object_slots: 1,
                    ..nexa_runtime::HostReturnRequirements::ZERO
                }
                .checked_add(match #source {
                    ::std::result::Result::Ok(value) => {
                        let _ = &value;
                        #success
                    },
                    ::std::result::Result::Err(error) => {
                        let _ = &error;
                        #error
                    },
                })?
            }
        }
        ResolvedTypeKind::Named(named) => match named.kind {
            NamedAbiKind::Handle => quote!(nexa_runtime::HostReturnRequirements::ZERO),
            NamedAbiKind::Struct => {
                let ident = &model
                    .structure(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_requirements(#source)?)
            }
            NamedAbiKind::Enum => {
                let ident = &model
                    .enumeration(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_requirements(#source)?)
            }
        },
    })
}

fn encode_runtime_value(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    source: TokenStream,
    writer: TokenStream,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => quote!(nexa_runtime::RuntimeValue::I32(#source)),
        ResolvedTypeKind::I64 => quote!(nexa_runtime::RuntimeValue::I64(#source)),
        ResolvedTypeKind::F32 => quote!(nexa_runtime::RuntimeValue::F32((#source).to_bits())),
        ResolvedTypeKind::F64 => quote!(nexa_runtime::RuntimeValue::F64((#source).to_bits())),
        ResolvedTypeKind::Bool => quote!(nexa_runtime::RuntimeValue::Bool(#source)),
        ResolvedTypeKind::Rune => quote!(nexa_runtime::RuntimeValue::Rune((#source) as u32)),
        ResolvedTypeKind::String => quote!(#writer.write_string(#source)?),
        ResolvedTypeKind::Token(_) => {
            quote!(nexa_runtime::RuntimeValue::ResourceToken((#source).into_raw()))
        }
        ResolvedTypeKind::Snapshot(_) => {
            quote!(nexa_runtime::RuntimeValue::Snapshot((#source).into_raw()))
        }
        ResolvedTypeKind::Array(inner) => {
            let type_id = nexa_bytecode::array_type(binding_value_type(inner)).0;
            let element_type = runtime_value_type(inner);
            let nested = encode_runtime_value(model, inner, quote!(value), writer.clone())?;
            let begin = if uses_compact_named_reference(inner) {
                quote!(#writer.begin_reference_array(
                    nexa_runtime::StableId(#type_id),
                    #element_type,
                    (#source).len(),
                )?)
            } else {
                quote!(#writer.begin_array(
                    nexa_runtime::StableId(#type_id),
                    #element_type,
                    (#source).len(),
                )?)
            };
            quote! {{
                let mut __nexa_array = #begin;
                for value in #source {
                    let __nexa_encoded = #nested;
                    #writer.push_array_value(&mut __nexa_array, __nexa_encoded)?;
                }
                #writer.finish_array(__nexa_array)?
            }}
        }
        ResolvedTypeKind::Buffer(inner) => {
            let type_id = nexa_bytecode::buffer_type(binding_value_type(inner)).0;
            let element_type = runtime_value_type(inner);
            let nested = encode_runtime_value(model, inner, quote!(value), writer.clone())?;
            let begin = if uses_compact_named_reference(inner) {
                quote!(#writer.begin_reference_buffer(
                    nexa_runtime::StableId(#type_id),
                    #element_type,
                    (#source).len(),
                )?)
            } else {
                quote!(#writer.begin_buffer(
                    nexa_runtime::StableId(#type_id),
                    #element_type,
                    (#source).len(),
                )?)
            };
            quote! {{
                let mut __nexa_buffer = #begin;
                for value in (#source).into_vec() {
                    let __nexa_encoded = #nested;
                    #writer.push_buffer_value(&mut __nexa_buffer, __nexa_encoded)?;
                }
                #writer.finish_buffer(__nexa_buffer)?
            }}
        }
        ResolvedTypeKind::Option(inner) => {
            let metadata = nexa_bytecode::option_type(binding_value_type(inner));
            let none = &metadata.variants[0];
            let some = &metadata.variants[1];
            let type_id = metadata.type_id.0;
            let none_id = none.stable_id.0;
            let none_tag = none.tag;
            let some_id = some.stable_id.0;
            let some_tag = some.tag;
            let nested = encode_runtime_value(model, inner, quote!(value), writer.clone())?;
            quote! {
                match #source {
                    ::std::option::Option::Some(value) => {
                        let __nexa_payload = #nested;
                        #writer.write_enum(
                            nexa_runtime::StableId(#type_id),
                            nexa_runtime::StableId(#some_id),
                            #some_tag,
                            ::std::option::Option::Some(__nexa_payload),
                        )?
                    }
                    ::std::option::Option::None => #writer.write_enum(
                        nexa_runtime::StableId(#type_id),
                        nexa_runtime::StableId(#none_id),
                        #none_tag,
                        ::std::option::Option::None,
                    )?,
                }
            }
        }
        ResolvedTypeKind::Result(success, error) => {
            let metadata =
                nexa_bytecode::result_type(binding_value_type(success), binding_value_type(error));
            let ok = &metadata.variants[0];
            let err = &metadata.variants[1];
            let type_id = metadata.type_id.0;
            let ok_id = ok.stable_id.0;
            let ok_tag = ok.tag;
            let err_id = err.stable_id.0;
            let err_tag = err.tag;
            let success = encode_runtime_value(model, success, quote!(value), writer.clone())?;
            let error = encode_runtime_value(model, error, quote!(error), writer.clone())?;
            quote! {
                match #source {
                    ::std::result::Result::Ok(value) => {
                        let __nexa_payload = #success;
                        #writer.write_enum(
                            nexa_runtime::StableId(#type_id),
                            nexa_runtime::StableId(#ok_id),
                            #ok_tag,
                            ::std::option::Option::Some(__nexa_payload),
                        )?
                    }
                    ::std::result::Result::Err(error) => {
                        let __nexa_payload = #error;
                        #writer.write_enum(
                            nexa_runtime::StableId(#type_id),
                            nexa_runtime::StableId(#err_id),
                            #err_tag,
                            ::std::option::Option::Some(__nexa_payload),
                        )?
                    }
                }
            }
        }
        ResolvedTypeKind::Named(named) => match named.kind {
            NamedAbiKind::Handle => {
                let type_id = named.stable_id.0;
                quote! {
                    nexa_runtime::RuntimeValue::Opaque {
                        value: (#source).0,
                        type_id: nexa_runtime::StableId(#type_id),
                    }
                }
            }
            NamedAbiKind::Struct => {
                let ident = &model
                    .structure(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_encode_runtime(#source, #writer)?)
            }
            NamedAbiKind::Enum => {
                let ident = &model
                    .enumeration(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_encode_runtime(#source, #writer)?)
            }
        },
    })
}

fn uses_compact_named_reference(ty: &ResolvedTypeRef) -> bool {
    match &ty.kind {
        ResolvedTypeKind::Array(_)
        | ResolvedTypeKind::Buffer(_)
        | ResolvedTypeKind::Option(_)
        | ResolvedTypeKind::Result(_, _) => true,
        ResolvedTypeKind::Named(named) => matches!(named.kind, NamedAbiKind::Enum),
        ResolvedTypeKind::I32
        | ResolvedTypeKind::I64
        | ResolvedTypeKind::F32
        | ResolvedTypeKind::F64
        | ResolvedTypeKind::Bool
        | ResolvedTypeKind::Rune
        | ResolvedTypeKind::String
        | ResolvedTypeKind::Token(_)
        | ResolvedTypeKind::Snapshot(_) => false,
    }
}

fn decode_host_value_ref(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    source: TokenStream,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => quote!(#source.i32()?),
        ResolvedTypeKind::I64 => quote!(#source.i64()?),
        ResolvedTypeKind::F32 => quote!(#source.f32()?),
        ResolvedTypeKind::F64 => quote!(#source.f64()?),
        ResolvedTypeKind::Bool => quote!(#source.bool()?),
        ResolvedTypeKind::Rune => quote!(#source.rune()?),
        ResolvedTypeKind::String => quote!(#source.str_ref()?.as_str()),
        ResolvedTypeKind::Token(target) => {
            let ident = model
                .handle(target.stable_id)
                .and_then(|handle| handle.token_wrapper_ident.as_ref())
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: target.source_name.clone(),
                    span: ty.span,
                })?;
            quote! {
                match #source.runtime_value() {
                    nexa_runtime::RuntimeValue::ResourceToken(value) =>
                        #ident::try_from_raw(value)?,
                    _ => return ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
                }
            }
        }
        ResolvedTypeKind::Snapshot(target) => {
            let ident = model
                .structure(target.stable_id)
                .and_then(|structure| structure.snapshot_names.as_ref())
                .map(|names| &names.wrapper_ident)
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: target.source_name.clone(),
                    span: ty.span,
                })?;
            quote! {
                match #source.runtime_value() {
                    nexa_runtime::RuntimeValue::Snapshot(value) =>
                        #ident::try_from(value)
                            .map_err(|_| nexa_runtime::HostTrap::Type)?,
                    _ => return ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
                }
            }
        }
        ResolvedTypeKind::Array(inner) => {
            let type_id = nexa_bytecode::array_type(binding_value_type(inner)).0;
            let nested = decode_host_value_ref(model, inner, quote!(__nexa_value))?;
            quote! {
                __NexaArrayRef::__nexa_from_runtime(
                    #source.array_ref(nexa_runtime::StableId(#type_id))?,
                    |__nexa_value| {
                        ::std::result::Result::Ok(#nested)
                    },
                )
            }
        }
        ResolvedTypeKind::Buffer(inner) => {
            let type_id = nexa_bytecode::buffer_type(binding_value_type(inner)).0;
            let nested = decode_host_value_ref(model, inner, quote!(__nexa_value))?;
            quote! {
                __NexaBufferRef::__nexa_from_runtime(
                    #source.buffer_ref(nexa_runtime::StableId(#type_id))?,
                    |__nexa_value| {
                        ::std::result::Result::Ok(#nested)
                    },
                )
            }
        }
        ResolvedTypeKind::Option(inner) => {
            let metadata = nexa_bytecode::option_type(binding_value_type(inner));
            let type_id = metadata.type_id.0;
            let none_id = metadata.variants[0].stable_id.0;
            let none_tag = metadata.variants[0].tag;
            let some_id = metadata.variants[1].stable_id.0;
            let some_tag = metadata.variants[1].tag;
            let nested = decode_host_value_ref(
                model,
                inner,
                quote!(__nexa_enum.payload().ok_or(nexa_runtime::HostTrap::Type)?),
            )?;
            quote! {{
                let __nexa_enum =
                    #source.enum_ref(nexa_runtime::StableId(#type_id))?;
                match (__nexa_enum.variant(), __nexa_enum.tag()) {
                    (nexa_runtime::StableId(#none_id), #none_tag)
                        if __nexa_enum.payload().is_none() =>
                            ::std::option::Option::None,
                    (nexa_runtime::StableId(#some_id), #some_tag) =>
                        ::std::option::Option::Some(#nested),
                    _ => return ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
                }
            }}
        }
        ResolvedTypeKind::Result(success, error) => {
            let metadata =
                nexa_bytecode::result_type(binding_value_type(success), binding_value_type(error));
            let type_id = metadata.type_id.0;
            let ok_id = metadata.variants[0].stable_id.0;
            let ok_tag = metadata.variants[0].tag;
            let err_id = metadata.variants[1].stable_id.0;
            let err_tag = metadata.variants[1].tag;
            let success = decode_host_value_ref(
                model,
                success,
                quote!(__nexa_enum.payload().ok_or(nexa_runtime::HostTrap::Type)?),
            )?;
            let error = decode_host_value_ref(
                model,
                error,
                quote!(__nexa_enum.payload().ok_or(nexa_runtime::HostTrap::Type)?),
            )?;
            quote! {{
                let __nexa_enum =
                    #source.enum_ref(nexa_runtime::StableId(#type_id))?;
                match (__nexa_enum.variant(), __nexa_enum.tag()) {
                    (nexa_runtime::StableId(#ok_id), #ok_tag) =>
                        ::std::result::Result::Ok(#success),
                    (nexa_runtime::StableId(#err_id), #err_tag) =>
                        ::std::result::Result::Err(#error),
                    _ => return ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
                }
            }}
        }
        ResolvedTypeKind::Named(named) => {
            let type_id = named.stable_id.0;
            let ident = validated_ident(&named.rust_name);
            match named.kind {
                NamedAbiKind::Handle => quote! {
                    match #source.runtime_value() {
                        nexa_runtime::RuntimeValue::Opaque { value, type_id }
                            if type_id == nexa_runtime::StableId(#type_id) =>
                            #ident(value),
                        _ => return ::std::result::Result::Err(
                            nexa_runtime::HostTrap::Type
                        ),
                    }
                },
                NamedAbiKind::Struct => {
                    let ref_ident = &model
                        .structure(named.stable_id)
                        .ok_or_else(|| CodegenError::UnknownNamedType {
                            name: named.source_name.clone(),
                            span: ty.span,
                        })?
                        .borrowed_ref_ident;
                    quote! {
                        #ref_ident::__nexa_from_runtime(
                            #source.struct_ref(nexa_runtime::StableId(#type_id))?
                        )?
                    }
                }
                NamedAbiKind::Enum => {
                    let ref_ident = &model
                        .enumeration(named.stable_id)
                        .ok_or_else(|| CodegenError::UnknownNamedType {
                            name: named.source_name.clone(),
                            span: ty.span,
                        })?
                        .borrowed_ref_ident;
                    quote! {
                        #ref_ident::__nexa_from_runtime(
                            #source.enum_ref(nexa_runtime::StableId(#type_id))?
                        )?
                    }
                }
            }
        }
    })
}

fn generate_host_surface(model: &BindingModel) -> Result<TokenStream, CodegenError> {
    let trait_ident = &model.host_trait_ident;
    let methods = model
        .host_functions
        .iter()
        .map(|function| generate_host_trait_method(model, function, false))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let stub_methods = model
        .host_functions
        .iter()
        .map(|function| generate_host_trait_method(model, function, true))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let completion_tickets = model
        .host_functions
        .iter()
        .filter(|function| function.is_async)
        .map(|function| generate_completion_ticket(model, function))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let dispatch_arms = model
        .host_functions
        .iter()
        .enumerate()
        .map(|(index, function)| generate_host_dispatch_arm(model, index, function))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let function_authorities = model
        .host_functions
        .iter()
        .map(generate_host_function_authority)
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let function_authority_arms =
        model
            .host_functions
            .iter()
            .enumerate()
            .map(|(index, function)| {
                let stable_id = function.identity.stable_id.0;
                let slot = u32::try_from(index).expect("Contract host function count fits u32");
                quote!(
                    nexa_runtime::StableId(#stable_id) =>
                    ::std::option::Option::Some(
                        nexa_runtime::ResolvedHostFunction::new(
                            nexa_runtime::HostFunctionSlot::new(#slot),
                            &HOST_FUNCTION_AUTHORITIES[#index],
                        )
                    )
                )
            });
    let contract_runtime_id = model.contract_runtime_id.0;
    let authority_count = model.host_functions.len();

    Ok(quote! {
        pub trait #trait_ident {
            #(#methods)*
        }

        pub struct GeneratedHostStub;

        impl #trait_ident for GeneratedHostStub {
            #(#stub_methods)*
        }

        #(#completion_tickets)*

        pub static HOST_FUNCTION_AUTHORITIES:
            ::std::sync::LazyLock<[nexa_runtime::HostFunctionAuthority; #authority_count]> =
            ::std::sync::LazyLock::new(|| [
                #(#function_authorities),*
            ]);

        pub struct GeneratedHostRegistry<H> {
            pub host: H,
        }

        impl<H> GeneratedHostRegistry<H> {
            pub const fn new(host: H) -> Self {
                Self { host }
            }
        }

        impl<H: #trait_ident> nexa_runtime::HostRegistry for GeneratedHostRegistry<H> {
            fn contract_runtime_id(&self) -> Option<nexa_runtime::StableId> {
                ::std::option::Option::Some(nexa_runtime::StableId(#contract_runtime_id))
            }

            fn resolve_function(
                &self,
                id: nexa_runtime::StableId,
            ) -> Option<nexa_runtime::ResolvedHostFunction<'_>> {
                match id {
                    #(#function_authority_arms),*,
                    _ => ::std::option::Option::None,
                }
            }

            fn call_runtime(
                &mut self,
                slot: nexa_runtime::HostFunctionSlot,
                context: &mut nexa_runtime::ResourceContext<'_>,
                args: nexa_runtime::RuntimeHostArgs<'_>,
            ) -> ::std::result::Result<
                nexa_runtime::HostCallOutcome,
                nexa_runtime::HostTrap,
            > {
                match slot.index() {
                    #(#dispatch_arms),*,
                    _ => ::std::result::Result::Err(
                        nexa_runtime::HostTrap::InvalidFunctionSlot(slot)
                    ),
                }
            }
        }

        pub fn registry<H: #trait_ident + 'static>(
            host: H,
        ) -> ::std::boxed::Box<dyn nexa_runtime::HostRegistry> {
            ::std::boxed::Box::new(GeneratedHostRegistry::new(host))
        }
    })
}

fn generate_host_function_authority(
    function: &BindingFunction,
) -> Result<TokenStream, CodegenError> {
    let Some(contract) = &function.host_contract else {
        return Err(CodegenError::InvalidAsyncHostContract {
            name: function.identity.source_name.clone(),
            span: function.identity.source_origin,
        });
    };
    let stable_id = function.identity.stable_id.0;
    let declaration_fingerprint = function.declaration_fingerprint.iter();
    let parameters = contract.parameters.iter().copied().map(value_type_tokens);
    let result = option_value_type_tokens(contract.result);
    let mode = match contract.mode {
        nexa_bytecode::HostCallMode::Immediate => quote!(nexa_runtime::HostCallMode::Immediate),
        nexa_bytecode::HostCallMode::Async => quote!(nexa_runtime::HostCallMode::Async),
    };
    let async_result = contract.async_result.map_or_else(
        || quote!(::std::option::Option::None),
        |result| {
            let result_type = result.result_type.0;
            let success = value_type_tokens(result.success);
            let error = value_type_tokens(result.error);
            let cancel_policy = match result.cancel_policy {
                nexa_bytecode::CancelPolicy::ReturnError => {
                    quote!(nexa_runtime::CancelPolicy::ReturnError)
                }
                nexa_bytecode::CancelPolicy::CancelTask => {
                    quote!(nexa_runtime::CancelPolicy::CancelTask)
                }
            };
            let abandon_policy = match result.abandon_policy {
                nexa_bytecode::AbandonPolicy::ReturnError => {
                    quote!(nexa_runtime::AbandonPolicy::ReturnError)
                }
                nexa_bytecode::AbandonPolicy::Trap => {
                    quote!(nexa_runtime::AbandonPolicy::Trap)
                }
            };
            let cancel_error = option_u32_tokens(result.cancel_error);
            let abandon_error = option_u32_tokens(result.abandon_error);
            quote! {
                ::std::option::Option::Some(nexa_runtime::AsyncResultType {
                    result_type: nexa_runtime::StableId(#result_type),
                    success: #success,
                    error: #error,
                    cancel_policy: #cancel_policy,
                    abandon_policy: #abandon_policy,
                    cancel_error: #cancel_error,
                    abandon_error: #abandon_error,
                })
            }
        },
    );
    let fuel_cost = function.fuel_cost;
    let capabilities = function.capabilities.iter();
    Ok(quote! {
        nexa_runtime::HostFunctionAuthority::new(
            nexa_runtime::StableId(#stable_id),
            [#(#declaration_fingerprint),*],
            &[#(#parameters),*],
            #result,
            #mode,
            #fuel_cost,
            #async_result,
            &[#(#capabilities),*],
        )
    })
}

fn value_type_tokens(value: ValueType) -> TokenStream {
    match value {
        ValueType::I32 => quote!(nexa_runtime::ValueType::I32),
        ValueType::I64 => quote!(nexa_runtime::ValueType::I64),
        ValueType::F32 => quote!(nexa_runtime::ValueType::F32),
        ValueType::F64 => quote!(nexa_runtime::ValueType::F64),
        ValueType::Bool => quote!(nexa_runtime::ValueType::Bool),
        ValueType::Rune => quote!(nexa_runtime::ValueType::Rune),
        ValueType::String => quote!(nexa_runtime::ValueType::String),
        ValueType::Ref => quote!(nexa_runtime::ValueType::Ref),
        ValueType::Named(id) => {
            let id = id.0;
            quote!(nexa_runtime::ValueType::Named(nexa_runtime::StableId(#id)))
        }
    }
}

fn option_value_type_tokens(value: Option<ValueType>) -> TokenStream {
    value.map_or_else(
        || quote!(::std::option::Option::None),
        |value| {
            let value = value_type_tokens(value);
            quote!(::std::option::Option::Some(#value))
        },
    )
}

fn option_u32_tokens(value: Option<u32>) -> TokenStream {
    value.map_or_else(
        || quote!(::std::option::Option::None),
        |value| quote!(::std::option::Option::Some(#value)),
    )
}

fn generate_host_trait_method(
    model: &BindingModel,
    function: &BindingFunction,
    stub: bool,
) -> Result<TokenStream, CodegenError> {
    let method_ident = &function.identity.rust_ident;
    let borrows = function
        .parameters
        .iter()
        .any(|parameter| type_borrows(&parameter.ty.source));
    let lifetime = borrows.then(|| quote!(<'a>));
    let parameter_lifetime = if borrows { quote!('a) } else { quote!('_) };
    let parameters = function
        .parameters
        .iter()
        .map(|parameter| {
            let ident = if stub {
                format_ident!("_{}", parameter.identity.rust_ident)
            } else {
                parameter.identity.rust_ident.clone()
            };
            let ty = input_rust_type(model, &parameter.ty.source, parameter_lifetime.clone())?;
            Ok(quote!(#ident: #ty))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let output = if function.is_async {
        quote!(nexa_runtime::HostRequestHandle)
    } else {
        function.result.as_ref().map_or_else(
            || Ok(quote!(())),
            |result| owned_rust_type(model, &result.source),
        )?
    };
    if stub {
        let message = [
            "host function is not registered: ",
            &function.identity.source_name,
        ]
        .concat();
        Ok(quote! {
            #[allow(clippy::too_many_arguments)]
            fn #method_ident #lifetime(
                &mut self,
                _context: &mut nexa_runtime::ResourceContext<'_>,
                #(#parameters),*
            ) -> ::std::result::Result<#output, HostError> {
                ::std::result::Result::Err(HostError(#message.into()))
            }
        })
    } else {
        Ok(quote! {
            #[allow(clippy::too_many_arguments)]
            fn #method_ident #lifetime(
                &mut self,
                context: &mut nexa_runtime::ResourceContext<'_>,
                #(#parameters),*
            ) -> ::std::result::Result<#output, HostError>;
        })
    }
}

fn generate_completion_ticket(
    model: &BindingModel,
    function: &BindingFunction,
) -> Result<TokenStream, CodegenError> {
    let ticket_ident = function.completion_ticket_ident.as_ref().ok_or_else(|| {
        CodegenError::GeneratedSyntax(format!(
            "async Host function `{}` has no resolved completion ticket name",
            function.identity.source_name
        ))
    })?;
    let completion = match function.result.as_ref().map(|result| &result.source.kind) {
        Some(ResolvedTypeKind::Result(success, error)) => {
            let success_ty = owned_rust_type(model, success)?;
            let error_ty = owned_rust_type(model, error)?;
            let success_payload = encode_completion_payload(model, success, quote!(value))?;
            let error_payload = encode_completion_payload(model, error, quote!(error))?;
            quote! {
                pub fn complete(
                    &mut self,
                    result: ::std::result::Result<#success_ty, #error_ty>,
                ) -> ::std::result::Result<(), nexa_runtime::HostRequestError> {
                    match result {
                        ::std::result::Result::Ok(value) => self.0.complete(#success_payload),
                        ::std::result::Result::Err(error) => self.0.fail(
                            nexa_runtime::HostErrorPayload::Value(#error_payload)
                        ),
                    }
                }
            }
        }
        Some(_) => {
            let result_ty = owned_rust_type(
                model,
                &function.result.as_ref().expect("matched result").source,
            )?;
            let payload = encode_completion_payload(
                model,
                &function.result.as_ref().expect("matched result").source,
                quote!(value),
            )?;
            quote! {
                pub fn complete(
                    &mut self,
                    value: #result_ty,
                ) -> ::std::result::Result<(), nexa_runtime::HostRequestError> {
                    self.0.complete(#payload)
                }
            }
        }
        None => quote! {
            pub fn complete(
                &mut self,
            ) -> ::std::result::Result<(), nexa_runtime::HostRequestError> {
                self.0.complete(nexa_runtime::HostPayload::Unit)
            }
        },
    };
    Ok(quote! {
        pub struct #ticket_ident(pub nexa_runtime::HostCompletionTicket);

        impl #ticket_ident {
            #completion

            pub fn cancelled(
                &mut self,
            ) -> ::std::result::Result<(), nexa_runtime::HostRequestError> {
                self.0.cancelled()
            }

            pub fn abandon(
                &mut self,
            ) -> ::std::result::Result<(), nexa_runtime::HostRequestError> {
                self.0.abandon()
            }
        }
    })
}

fn generate_host_dispatch_arm(
    model: &BindingModel,
    index: usize,
    function: &BindingFunction,
) -> Result<TokenStream, CodegenError> {
    let slot = u32::try_from(index).expect("Contract host function count fits u32");
    let method_ident = &function.identity.rust_ident;
    let arity = function.parameters.len();
    let arity_mismatch = if arity == 0 {
        quote!(!args.is_empty())
    } else {
        quote!(args.len() != #arity)
    };
    let decoded = function
        .parameters
        .iter()
        .enumerate()
        .map(|(index, parameter)| {
            let ident = &parameter.identity.rust_ident;
            let value = decode_host_argument(model, &parameter.ty.source, index)?;
            Ok(quote!(let #ident = #value;))
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let argument_idents = function
        .parameters
        .iter()
        .map(|parameter| &parameter.identity.rust_ident);
    let outcome = if function.is_async {
        quote!(::std::result::Result::Ok(
            nexa_runtime::HostCallOutcome::Pending(result)
        ))
    } else if let Some(result) = &function.result {
        if return_requires_writer(&result.source) {
            let requirement_source = requirements_source_for_place(&result.source, quote!(result));
            let requirements = requirements_for_type(model, &result.source, requirement_source)?;
            let encoded = encode_runtime_value(
                model,
                &result.source,
                quote!(result),
                quote!(__nexa_transaction),
            )?;
            quote! {
                let __nexa_requirements = #requirements;
                let mut __nexa_return = args.return_transaction(__nexa_requirements)?;
                let __nexa_value = {
                    let __nexa_transaction = &mut __nexa_return;
                    #encoded
                };
                let __nexa_value = __nexa_return.commit(__nexa_value)?;
                ::std::result::Result::Ok(
                    nexa_runtime::HostCallOutcome::RuntimeImmediate(__nexa_value)
                )
            }
        } else {
            let encoded =
                encode_runtime_value(model, &result.source, quote!(result), quote!(__nexa_unused))?;
            quote! {
                ::std::result::Result::Ok(
                    nexa_runtime::HostCallOutcome::RuntimeImmediate(#encoded)
                )
            }
        }
    } else {
        quote! {
            let _ = result;
            ::std::result::Result::Ok(nexa_runtime::HostCallOutcome::RuntimeImmediate(
                nexa_runtime::RuntimeValue::Unit
            ))
        }
    };
    Ok(quote! {
        #slot => {
            if #arity_mismatch {
                return ::std::result::Result::Err(nexa_runtime::HostTrap::Arity);
            }
            #(#decoded)*
            let result = ::std::panic::catch_unwind(
                ::std::panic::AssertUnwindSafe(|| {
                    self.host.#method_ident(context, #(#argument_idents),*)
                }),
            )
            .map_err(|_| nexa_runtime::HostTrap::Panicked)?
            .map_err(|error| {
                nexa_runtime::HostTrap::Host(
                    nexa_runtime::RuntimeMessage::inline(&error.0)
                )
            })?;
            #outcome
        }
    })
}

fn decode_host_argument(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    index: usize,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => quote!(args.i32(#index)?),
        ResolvedTypeKind::I64 => quote!(args.i64(#index)?),
        ResolvedTypeKind::F32 => quote!(args.f32(#index)?),
        ResolvedTypeKind::F64 => quote!(args.f64(#index)?),
        ResolvedTypeKind::Bool => quote!(args.bool(#index)?),
        ResolvedTypeKind::Rune => quote!(args.rune(#index)?),
        ResolvedTypeKind::String => quote!(args.str_ref(#index)?.as_str()),
        ResolvedTypeKind::Token(named) => {
            let ident = model
                .handle(named.stable_id)
                .and_then(|handle| handle.token_wrapper_ident.as_ref())
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: named.source_name.clone(),
                    span: ty.span,
                })?;
            let content_type = named.stable_id.0;
            quote! {
                #ident::try_from_raw(
                    args.typed_token(
                        #index,
                        nexa_runtime::StableId(#content_type),
                    )?
                )?
            }
        }
        ResolvedTypeKind::Snapshot(named) => {
            let ident = model
                .structure(named.stable_id)
                .and_then(|structure| structure.snapshot_names.as_ref())
                .map(|names| &names.wrapper_ident)
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: named.source_name.clone(),
                    span: ty.span,
                })?;
            quote! {
                #ident::try_from(args.snapshot(#index)?)
                    .map_err(|_| nexa_runtime::HostTrap::Type)?
            }
        }
        _ => decode_host_value_ref(model, ty, quote!(args.value_ref(#index)?))?,
    })
}

fn return_requires_writer(ty: &ResolvedTypeRef) -> bool {
    match &ty.kind {
        ResolvedTypeKind::String
        | ResolvedTypeKind::Array(_)
        | ResolvedTypeKind::Buffer(_)
        | ResolvedTypeKind::Option(_)
        | ResolvedTypeKind::Result(_, _) => true,
        ResolvedTypeKind::Named(named) => match named.kind {
            NamedAbiKind::Handle => false,
            NamedAbiKind::Struct | NamedAbiKind::Enum => true,
        },
        ResolvedTypeKind::I32
        | ResolvedTypeKind::I64
        | ResolvedTypeKind::F32
        | ResolvedTypeKind::F64
        | ResolvedTypeKind::Bool
        | ResolvedTypeKind::Rune
        | ResolvedTypeKind::Token(_)
        | ResolvedTypeKind::Snapshot(_) => false,
    }
}

fn encode_completion_payload(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    source: TokenStream,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => quote!(nexa_runtime::HostPayload::I32(#source)),
        ResolvedTypeKind::I64 => quote!(nexa_runtime::HostPayload::I64(#source)),
        ResolvedTypeKind::F32 => quote!(nexa_runtime::HostPayload::F32((#source).to_bits())),
        ResolvedTypeKind::F64 => quote!(nexa_runtime::HostPayload::F64((#source).to_bits())),
        ResolvedTypeKind::Bool => quote!(nexa_runtime::HostPayload::Bool(#source)),
        ResolvedTypeKind::Rune => quote!(nexa_runtime::HostPayload::Rune((#source) as u32)),
        ResolvedTypeKind::String => quote!(nexa_runtime::HostPayload::String(#source)),
        ResolvedTypeKind::Token(_) => {
            quote!(nexa_runtime::HostPayload::Token((#source).into_raw()))
        }
        ResolvedTypeKind::Snapshot(_) => {
            quote!(nexa_runtime::HostPayload::Snapshot((#source).into_raw()))
        }
        ResolvedTypeKind::Array(inner) => {
            let nested = encode_completion_payload(model, inner, quote!(value))?;
            quote! {
                nexa_runtime::HostPayload::Array(
                    nexa_runtime::CopyBuffer::new(
                        (#source).into_iter().map(|value| #nested).collect()
                    )
                )
            }
        }
        ResolvedTypeKind::Buffer(inner) => {
            let nested = encode_completion_payload(model, inner, quote!(value))?;
            quote! {
                nexa_runtime::HostPayload::Buffer(
                    nexa_runtime::CopyBuffer::new(
                        (#source)
                            .into_vec()
                            .into_iter()
                            .map(|value| #nested)
                            .collect()
                    )
                )
            }
        }
        ResolvedTypeKind::Option(inner) => {
            let metadata = nexa_bytecode::option_type(binding_value_type(inner));
            let type_id = metadata.type_id.0;
            let none_id = metadata.variants[0].stable_id.0;
            let none_tag = metadata.variants[0].tag;
            let some_id = metadata.variants[1].stable_id.0;
            let some_tag = metadata.variants[1].tag;
            let nested = encode_completion_payload(model, inner, quote!(value))?;
            quote! {
                match #source {
                    ::std::option::Option::Some(value) =>
                        nexa_runtime::HostPayload::Enum {
                        type_id: nexa_runtime::StableId(#type_id),
                        variant: nexa_runtime::StableId(#some_id),
                        tag: #some_tag,
                        payload: ::std::option::Option::Some(
                            ::std::boxed::Box::new(#nested)
                        ),
                    },
                    ::std::option::Option::None =>
                        nexa_runtime::HostPayload::Enum {
                        type_id: nexa_runtime::StableId(#type_id),
                        variant: nexa_runtime::StableId(#none_id),
                        tag: #none_tag,
                        payload: ::std::option::Option::None,
                    },
                }
            }
        }
        ResolvedTypeKind::Result(success, error) => {
            let metadata =
                nexa_bytecode::result_type(binding_value_type(success), binding_value_type(error));
            let type_id = metadata.type_id.0;
            let ok_id = metadata.variants[0].stable_id.0;
            let ok_tag = metadata.variants[0].tag;
            let err_id = metadata.variants[1].stable_id.0;
            let err_tag = metadata.variants[1].tag;
            let success = encode_completion_payload(model, success, quote!(value))?;
            let error = encode_completion_payload(model, error, quote!(error))?;
            quote! {
                match #source {
                    ::std::result::Result::Ok(value) =>
                        nexa_runtime::HostPayload::Enum {
                        type_id: nexa_runtime::StableId(#type_id),
                        variant: nexa_runtime::StableId(#ok_id),
                        tag: #ok_tag,
                        payload: ::std::option::Option::Some(
                            ::std::boxed::Box::new(#success)
                        ),
                    },
                    ::std::result::Result::Err(error) =>
                        nexa_runtime::HostPayload::Enum {
                        type_id: nexa_runtime::StableId(#type_id),
                        variant: nexa_runtime::StableId(#err_id),
                        tag: #err_tag,
                        payload: ::std::option::Option::Some(
                            ::std::boxed::Box::new(#error)
                        ),
                    },
                }
            }
        }
        ResolvedTypeKind::Named(named) => match named.kind {
            NamedAbiKind::Handle => quote!(nexa_runtime::HostPayload::Opaque((#source).0)),
            NamedAbiKind::Struct => {
                let ident = &model
                    .structure(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_completion_payload(#source))
            }
            NamedAbiKind::Enum => {
                let ident = &model
                    .enumeration(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_completion_payload(#source))
            }
        },
    })
}

fn generate_nexa_surface(model: &BindingModel) -> Result<TokenStream, CodegenError> {
    let markers = model
        .nexa_functions
        .iter()
        .enumerate()
        .map(|(contract_slot, function)| generate_nexa_marker(model, function, contract_slot))
        .collect::<Result<Vec<_>, CodegenError>>()?;
    Ok(quote!(#(#markers)*))
}

fn generate_nexa_marker(
    model: &BindingModel,
    function: &BindingFunction,
    contract_slot: usize,
) -> Result<TokenStream, CodegenError> {
    let marker_ident = &function.marker_ident;
    let args_ident = &function.args_ident;
    let output_ident = &function.output_ident;
    let name = &function.identity.source_name;
    let stable_id = function.identity.stable_id.0;
    let effect = if function.is_async {
        quote!(nexa_runtime::FunctionEffect::Task)
    } else {
        quote!(nexa_runtime::FunctionEffect::Ordinary)
    };
    let args_definition = if function.parameters.is_empty() {
        quote! {
            #[derive(Clone, Debug, Default, PartialEq)]
            pub struct #args_ident;
        }
    } else {
        let fields = function
            .parameters
            .iter()
            .map(|parameter| {
                let ident = &parameter.identity.rust_ident;
                let ty = owned_rust_type(model, &parameter.ty.source)?;
                Ok(quote!(pub #ident: #ty))
            })
            .collect::<Result<Vec<_>, CodegenError>>()?;
        quote! {
            #[derive(Clone, Debug, PartialEq)]
            pub struct #args_ident {
                #(#fields),*
            }
        }
    };
    let output_type = function.result.as_ref().map_or_else(
        || Ok(quote!(())),
        |result| owned_rust_type(model, &result.source),
    )?;
    let parameter_types = function
        .parameters
        .iter()
        .map(|parameter| runtime_value_type(&parameter.ty.source))
        .collect::<Vec<_>>();
    let result_type = function
        .result
        .as_ref()
        .map(|result| runtime_value_type(&result.source));
    let result_signature = result_type.map_or_else(
        || quote!(::std::option::Option::None),
        |result| quote!(::std::option::Option::Some(#result)),
    );
    let requirement_additions = function
        .parameters
        .iter()
        .map(|parameter| {
            let ident = &parameter.identity.rust_ident;
            let requirement_source =
                requirements_source_for_place(&parameter.ty.source, quote!(args.#ident));
            let requirement =
                requirements_for_type(model, &parameter.ty.source, requirement_source)?;
            Ok(quote! {
                __nexa_requirements =
                    __nexa_requirements.checked_add(#requirement)?;
            })
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let encoded_args = function
        .parameters
        .iter()
        .map(|parameter| {
            let ident = &parameter.identity.rust_ident;
            encode_runtime_value(
                model,
                &parameter.ty.source,
                quote!(args.#ident.clone()),
                quote!(writer),
            )
        })
        .collect::<Result<Vec<_>, CodegenError>>()?;
    let decoded_output = if let Some(result) = &function.result {
        decode_script_output_result(model, &result.source, quote!(reader.value(value)))?
    } else {
        quote! {
            ::std::result::Result::Ok(match value {
                nexa_runtime::RuntimeValue::Unit => (),
                _ => return ::std::result::Result::Err(
                    nexa_runtime::ScriptCallError::OutputDecoding
                ),
            })
        }
    };
    Ok(quote! {
        #args_definition
        pub type #output_ident = #output_type;
        pub enum #marker_ident {}

        impl #marker_ident {
            pub const NAME: &'static str = #name;
            pub const STABLE_ID: nexa_runtime::StableId =
                nexa_runtime::StableId(#stable_id);
        }

        impl nexa_runtime::ScriptExport for #marker_ident {
            type Args = #args_ident;
            type Output = #output_ident;

            const STABLE_ID: nexa_runtime::StableId =
                nexa_runtime::StableId(#stable_id);
            const NAME: &'static str = #name;
            const CONTRACT_SLOT: usize = #contract_slot;
            const SIGNATURE: nexa_runtime::ScriptSignature =
                nexa_runtime::ScriptSignature::new(
                    &[#(#parameter_types),*],
                    #result_signature,
                );
            const EFFECT: nexa_runtime::FunctionEffect = #effect;

            fn argument_requirements(
                args: &Self::Args,
            ) -> ::std::result::Result<
                nexa_runtime::ScriptArgumentRequirements,
                nexa_runtime::ScriptCallError,
            > {
                let _ = args;
                let mut __nexa_requirements =
                    nexa_runtime::ScriptArgumentRequirements::ZERO;
                #(#requirement_additions)*
                ::std::result::Result::Ok(__nexa_requirements)
            }

            #[allow(clippy::clone_on_copy)]
            fn encode_args(
                writer: &mut nexa_runtime::ScriptCallWriter<'_>,
                args: &Self::Args,
            ) -> ::std::result::Result<
                nexa_runtime::ScriptArguments,
                nexa_runtime::ScriptCallError,
            > {
                let _ = writer;
                let _ = args;
                nexa_runtime::ScriptArguments::try_from_array([#(#encoded_args),*])
            }

            fn decode_output(
                reader: &nexa_runtime::ScriptOutputReader<'_>,
                value: nexa_runtime::RuntimeValue,
            ) -> ::std::result::Result<
                Self::Output,
                nexa_runtime::ScriptCallError,
            > {
                let _ = reader;
                #decoded_output
            }
        }
    })
}

fn decode_script_output_result(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    source: TokenStream,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => quote! {
            #source
                .i32()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)
        },
        ResolvedTypeKind::I64 => quote! {
            #source
                .i64()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)
        },
        ResolvedTypeKind::F32 => quote! {
            #source
                .f32()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)
        },
        ResolvedTypeKind::F64 => quote! {
            #source
                .f64()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)
        },
        ResolvedTypeKind::Bool => quote! {
            #source
                .bool()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)
        },
        ResolvedTypeKind::Rune => quote! {
            #source
                .rune()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)
        },
        _ => {
            let decoded = decode_script_output(model, ty, source)?;
            quote!(::std::result::Result::Ok(#decoded))
        }
    })
}

fn decode_script_output(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    source: TokenStream,
) -> Result<TokenStream, CodegenError> {
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => quote! {
            #source
                .i32()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
        },
        ResolvedTypeKind::I64 => quote! {
            #source
                .i64()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
        },
        ResolvedTypeKind::F32 => quote! {
            #source
                .f32()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
        },
        ResolvedTypeKind::F64 => quote! {
            #source
                .f64()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
        },
        ResolvedTypeKind::Bool => quote! {
            #source
                .bool()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
        },
        ResolvedTypeKind::Rune => quote! {
            #source
                .rune()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
        },
        ResolvedTypeKind::String => quote! {
            #source
                .str_ref()
                .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?
                .as_str()
                .to_owned()
        },
        ResolvedTypeKind::Token(named) => {
            let ident = model
                .handle(named.stable_id)
                .and_then(|handle| handle.token_wrapper_ident.as_ref())
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: named.source_name.clone(),
                    span: ty.span,
                })?;
            quote! {
                match #source.runtime_value() {
                    nexa_runtime::RuntimeValue::ResourceToken(value) =>
                        #ident::try_from_raw(value)
                            .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?,
                    _ => return ::std::result::Result::Err(
                        nexa_runtime::ScriptCallError::OutputDecoding
                    ),
                }
            }
        }
        ResolvedTypeKind::Snapshot(named) => {
            let ident = model
                .structure(named.stable_id)
                .and_then(|structure| structure.snapshot_names.as_ref())
                .map(|names| &names.wrapper_ident)
                .ok_or_else(|| CodegenError::UnknownNamedType {
                    name: named.source_name.clone(),
                    span: ty.span,
                })?;
            quote! {
                match #source.runtime_value() {
                    nexa_runtime::RuntimeValue::Snapshot(value) =>
                        #ident::try_from(value)
                            .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?,
                    _ => return ::std::result::Result::Err(
                        nexa_runtime::ScriptCallError::OutputDecoding
                    ),
                }
            }
        }
        ResolvedTypeKind::Array(inner) | ResolvedTypeKind::Buffer(inner) => {
            let (type_id, accessor, buffer) = if matches!(ty.kind, ResolvedTypeKind::Array(_)) {
                (
                    nexa_bytecode::array_type(binding_value_type(inner)).0,
                    format_ident!("array_ref"),
                    false,
                )
            } else {
                (
                    nexa_bytecode::buffer_type(binding_value_type(inner)).0,
                    format_ident!("buffer_ref"),
                    true,
                )
            };
            let nested = decode_script_output(model, inner, quote!(__nexa_value))?;
            let collection = quote! {{
                let __nexa_collection =
                    #source
                        .#accessor(nexa_runtime::StableId(#type_id))
                        .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?;
                let mut __nexa_output =
                    ::std::vec::Vec::with_capacity(__nexa_collection.len());
                for __nexa_value in __nexa_collection.iter() {
                    __nexa_output.push(#nested);
                }
                __nexa_output
            }};
            if buffer {
                quote!(nexa_runtime::CopyBuffer::new(#collection))
            } else {
                collection
            }
        }
        ResolvedTypeKind::Option(inner) => {
            let metadata = nexa_bytecode::option_type(binding_value_type(inner));
            let type_id = metadata.type_id.0;
            let none_id = metadata.variants[0].stable_id.0;
            let none_tag = metadata.variants[0].tag;
            let some_id = metadata.variants[1].stable_id.0;
            let some_tag = metadata.variants[1].tag;
            let nested = decode_script_output(
                model,
                inner,
                quote!(
                    __nexa_enum
                        .payload()
                        .ok_or(nexa_runtime::ScriptCallError::OutputDecoding)?
                ),
            )?;
            quote! {{
                let __nexa_enum =
                    #source
                        .enum_ref(nexa_runtime::StableId(#type_id))
                        .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?;
                match (__nexa_enum.variant(), __nexa_enum.tag()) {
                    (nexa_runtime::StableId(#none_id), #none_tag)
                        if __nexa_enum.payload().is_none() =>
                            ::std::option::Option::None,
                    (nexa_runtime::StableId(#some_id), #some_tag) =>
                        ::std::option::Option::Some(#nested),
                    _ => return ::std::result::Result::Err(
                        nexa_runtime::ScriptCallError::OutputDecoding
                    ),
                }
            }}
        }
        ResolvedTypeKind::Result(success, error) => {
            let metadata =
                nexa_bytecode::result_type(binding_value_type(success), binding_value_type(error));
            let type_id = metadata.type_id.0;
            let ok_id = metadata.variants[0].stable_id.0;
            let ok_tag = metadata.variants[0].tag;
            let err_id = metadata.variants[1].stable_id.0;
            let err_tag = metadata.variants[1].tag;
            let success = decode_script_output(
                model,
                success,
                quote!(
                    __nexa_enum
                        .payload()
                        .ok_or(nexa_runtime::ScriptCallError::OutputDecoding)?
                ),
            )?;
            let error = decode_script_output(
                model,
                error,
                quote!(
                    __nexa_enum
                        .payload()
                        .ok_or(nexa_runtime::ScriptCallError::OutputDecoding)?
                ),
            )?;
            quote! {{
                let __nexa_enum =
                    #source
                        .enum_ref(nexa_runtime::StableId(#type_id))
                        .map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?;
                match (__nexa_enum.variant(), __nexa_enum.tag()) {
                    (nexa_runtime::StableId(#ok_id), #ok_tag) =>
                        ::std::result::Result::Ok(#success),
                    (nexa_runtime::StableId(#err_id), #err_tag) =>
                        ::std::result::Result::Err(#error),
                    _ => return ::std::result::Result::Err(
                        nexa_runtime::ScriptCallError::OutputDecoding
                    ),
                }
            }}
        }
        ResolvedTypeKind::Named(named) => {
            let ident = validated_ident(&named.rust_name);
            let type_id = named.stable_id.0;
            match named.kind {
                NamedAbiKind::Handle => quote! {
                    match #source.runtime_value() {
                        nexa_runtime::RuntimeValue::Opaque { value, type_id }
                            if type_id == nexa_runtime::StableId(#type_id) =>
                            #ident(value),
                        _ => return ::std::result::Result::Err(
                            nexa_runtime::ScriptCallError::OutputDecoding
                        ),
                    }
                },
                NamedAbiKind::Struct => {
                    model.structure(named.stable_id).ok_or_else(|| {
                        CodegenError::UnknownNamedType {
                            name: named.source_name.clone(),
                            span: ty.span,
                        }
                    })?;
                    quote!(#ident::__nexa_decode_script(#source)?)
                }
                NamedAbiKind::Enum => {
                    model.enumeration(named.stable_id).ok_or_else(|| {
                        CodegenError::UnknownNamedType {
                            name: named.source_name.clone(),
                            span: ty.span,
                        }
                    })?;
                    quote!(#ident::__nexa_decode_script(#source)?)
                }
            }
        }
    })
}

fn snapshot_encode(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    source: TokenStream,
    bytes: TokenStream,
    source_is_borrowed: bool,
) -> Result<TokenStream, CodegenError> {
    let source_ref = if source_is_borrowed {
        source.clone()
    } else {
        quote!(&(#source))
    };
    Ok(match &ty.kind {
        ResolvedTypeKind::I32 | ResolvedTypeKind::I64 => {
            if source_is_borrowed {
                quote!(#bytes.extend_from_slice(&(*#source).to_le_bytes());)
            } else {
                quote!(#bytes.extend_from_slice(&(#source).to_le_bytes());)
            }
        }
        ResolvedTypeKind::F32 | ResolvedTypeKind::F64 => {
            if source_is_borrowed {
                quote!(#bytes.extend_from_slice(&(*#source).to_bits().to_le_bytes());)
            } else {
                quote!(#bytes.extend_from_slice(&(#source).to_bits().to_le_bytes());)
            }
        }
        ResolvedTypeKind::Bool => {
            if source_is_borrowed {
                quote!(#bytes.push(u8::from(*#source));)
            } else {
                quote!(#bytes.push(u8::from(#source));)
            }
        }
        ResolvedTypeKind::Rune => {
            if source_is_borrowed {
                quote!(#bytes.extend_from_slice(&u32::from(*#source).to_le_bytes());)
            } else {
                quote!(#bytes.extend_from_slice(&u32::from(#source).to_le_bytes());)
            }
        }
        ResolvedTypeKind::String => quote! {
            let __nexa_len = u32::try_from((#source).len())
                .map_err(|_| HostError("snapshot string is too large".into()))?;
            #bytes.extend_from_slice(&__nexa_len.to_le_bytes());
            #bytes.extend_from_slice((#source).as_bytes());
        },
        ResolvedTypeKind::Array(inner) => {
            let nested = snapshot_encode(model, inner, quote!(__nexa_item), bytes.clone(), true)?;
            quote! {
                let __nexa_len = u32::try_from((#source).len())
                    .map_err(|_| HostError("snapshot array is too large".into()))?;
                #bytes.extend_from_slice(&__nexa_len.to_le_bytes());
                for __nexa_item in (#source).iter() {
                    #nested
                }
            }
        }
        ResolvedTypeKind::Buffer(inner) => {
            let nested = snapshot_encode(model, inner, quote!(__nexa_item), bytes.clone(), true)?;
            quote! {
                let __nexa_len = u32::try_from((#source).len())
                    .map_err(|_| HostError("snapshot buffer is too large".into()))?;
                #bytes.extend_from_slice(&__nexa_len.to_le_bytes());
                for __nexa_item in (#source).as_slice() {
                    #nested
                }
            }
        }
        ResolvedTypeKind::Option(inner) => {
            let nested = snapshot_encode(model, inner, quote!(__nexa_value), bytes.clone(), true)?;
            quote! {
                match #source_ref {
                    ::std::option::Option::Some(__nexa_value) => {
                        #bytes.push(1);
                        #nested
                    }
                    ::std::option::Option::None => #bytes.push(0),
                }
            }
        }
        ResolvedTypeKind::Result(success, error) => {
            let success =
                snapshot_encode(model, success, quote!(__nexa_value), bytes.clone(), true)?;
            let error = snapshot_encode(model, error, quote!(__nexa_error), bytes.clone(), true)?;
            quote! {
                match #source_ref {
                    ::std::result::Result::Ok(__nexa_value) => {
                        #bytes.push(0);
                        #success
                    }
                    ::std::result::Result::Err(__nexa_error) => {
                        #bytes.push(1);
                        #error
                    }
                }
            }
        }
        ResolvedTypeKind::Named(named) => match named.kind {
            NamedAbiKind::Handle => {
                quote!(#bytes.extend_from_slice(&(#source).0.to_le_bytes());)
            }
            NamedAbiKind::Struct => {
                let ident = &model
                    .structure(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_snapshot_encode(#source_ref, #bytes)?;)
            }
            NamedAbiKind::Enum => {
                let ident = &model
                    .enumeration(named.stable_id)
                    .ok_or_else(|| CodegenError::UnknownNamedType {
                        name: named.source_name.clone(),
                        span: ty.span,
                    })?
                    .identity
                    .rust_ident;
                quote!(#ident::__nexa_snapshot_encode(#source_ref, #bytes)?;)
            }
        },
        ResolvedTypeKind::Token(_) | ResolvedTypeKind::Snapshot(_) => quote! {
            return ::std::result::Result::Err(HostError(
                "host handles cannot be embedded in snapshots".into()
            ));
        },
    })
}

fn snapshot_decode(
    model: &BindingModel,
    ty: &ResolvedTypeRef,
    payload: TokenStream,
    cursor: TokenStream,
) -> Result<TokenStream, CodegenError> {
    fn fixed(
        payload: &TokenStream,
        cursor: &TokenStream,
        size: usize,
        conversion: TokenStream,
    ) -> TokenStream {
        quote! {{
            let __nexa_end = (#cursor)
                .checked_add(#size)
                .ok_or(nexa_runtime::HostTrap::Type)?;
            let __nexa_slice = #payload
                .get(#cursor..__nexa_end)
                .ok_or(nexa_runtime::HostTrap::Type)?;
            #cursor = __nexa_end;
            #conversion
        }}
    }

    Ok(match &ty.kind {
        ResolvedTypeKind::I32 => fixed(
            &payload,
            &cursor,
            4,
            quote! {
                i32::from_le_bytes(
                    __nexa_slice
                        .try_into()
                        .map_err(|_| nexa_runtime::HostTrap::Type)?
                )
            },
        ),
        ResolvedTypeKind::I64 => fixed(
            &payload,
            &cursor,
            8,
            quote! {
                i64::from_le_bytes(
                    __nexa_slice
                        .try_into()
                        .map_err(|_| nexa_runtime::HostTrap::Type)?
                )
            },
        ),
        ResolvedTypeKind::F32 => fixed(
            &payload,
            &cursor,
            4,
            quote! {
                f32::from_bits(u32::from_le_bytes(
                    __nexa_slice
                        .try_into()
                        .map_err(|_| nexa_runtime::HostTrap::Type)?
                ))
            },
        ),
        ResolvedTypeKind::F64 => fixed(
            &payload,
            &cursor,
            8,
            quote! {
                f64::from_bits(u64::from_le_bytes(
                    __nexa_slice
                        .try_into()
                        .map_err(|_| nexa_runtime::HostTrap::Type)?
                ))
            },
        ),
        ResolvedTypeKind::Bool => quote! {{
            let __nexa_value = *#payload
                .get(#cursor)
                .ok_or(nexa_runtime::HostTrap::Type)?;
            #cursor += 1;
            match __nexa_value {
                0 => false,
                1 => true,
                _ => return ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
            }
        }},
        ResolvedTypeKind::Rune => {
            let value = fixed(
                &payload,
                &cursor,
                4,
                quote! {
                    u32::from_le_bytes(
                        __nexa_slice
                            .try_into()
                            .map_err(|_| nexa_runtime::HostTrap::Type)?
                    )
                },
            );
            quote!(char::from_u32(#value).ok_or(nexa_runtime::HostTrap::Type)?)
        }
        ResolvedTypeKind::String => {
            let length = fixed(
                &payload,
                &cursor,
                4,
                quote! {
                    u32::from_le_bytes(
                        __nexa_slice
                            .try_into()
                            .map_err(|_| nexa_runtime::HostTrap::Type)?
                    )
                },
            );
            quote! {{
                let __nexa_len =
                    usize::try_from(#length).map_err(|_| nexa_runtime::HostTrap::Type)?;
                let __nexa_end = (#cursor)
                    .checked_add(__nexa_len)
                    .ok_or(nexa_runtime::HostTrap::Type)?;
                let __nexa_value = ::std::str::from_utf8(
                    #payload
                        .get(#cursor..__nexa_end)
                        .ok_or(nexa_runtime::HostTrap::Type)?
                )
                .map_err(|_| nexa_runtime::HostTrap::Type)?
                .to_owned();
                #cursor = __nexa_end;
                __nexa_value
            }}
        }
        ResolvedTypeKind::Array(inner) | ResolvedTypeKind::Buffer(inner) => {
            let length = fixed(
                &payload,
                &cursor,
                4,
                quote! {
                    u32::from_le_bytes(
                        __nexa_slice
                            .try_into()
                            .map_err(|_| nexa_runtime::HostTrap::Type)?
                    )
                },
            );
            let nested = snapshot_decode(model, inner, payload.clone(), cursor.clone())?;
            let values = quote! {{
                let __nexa_len =
                    usize::try_from(#length).map_err(|_| nexa_runtime::HostTrap::Type)?;
                let mut __nexa_values = ::std::vec::Vec::with_capacity(__nexa_len);
                for _ in 0..__nexa_len {
                    __nexa_values.push(#nested);
                }
                __nexa_values
            }};
            if matches!(ty.kind, ResolvedTypeKind::Buffer(_)) {
                quote!(nexa_runtime::CopyBuffer::new(#values))
            } else {
                values
            }
        }
        ResolvedTypeKind::Option(inner) => {
            let nested = snapshot_decode(model, inner, payload.clone(), cursor.clone())?;
            quote! {{
                let __nexa_tag = *#payload
                    .get(#cursor)
                    .ok_or(nexa_runtime::HostTrap::Type)?;
                #cursor += 1;
                match __nexa_tag {
                    0 => ::std::option::Option::None,
                    1 => ::std::option::Option::Some(#nested),
                    _ => return ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
                }
            }}
        }
        ResolvedTypeKind::Result(success, error) => {
            let success = snapshot_decode(model, success, payload.clone(), cursor.clone())?;
            let error = snapshot_decode(model, error, payload.clone(), cursor.clone())?;
            quote! {{
                let __nexa_tag = *#payload
                    .get(#cursor)
                    .ok_or(nexa_runtime::HostTrap::Type)?;
                #cursor += 1;
                match __nexa_tag {
                    0 => ::std::result::Result::Ok(#success),
                    1 => ::std::result::Result::Err(#error),
                    _ => return ::std::result::Result::Err(nexa_runtime::HostTrap::Type),
                }
            }}
        }
        ResolvedTypeKind::Named(named) => {
            let ident = validated_ident(&named.rust_name);
            match named.kind {
                NamedAbiKind::Handle => {
                    let value = fixed(
                        &payload,
                        &cursor,
                        8,
                        quote! {
                            u64::from_le_bytes(
                                __nexa_slice
                                    .try_into()
                                    .map_err(|_| nexa_runtime::HostTrap::Type)?
                            )
                        },
                    );
                    quote!(#ident(#value))
                }
                NamedAbiKind::Struct => {
                    model.structure(named.stable_id).ok_or_else(|| {
                        CodegenError::UnknownNamedType {
                            name: named.source_name.clone(),
                            span: ty.span,
                        }
                    })?;
                    quote!(#ident::__nexa_snapshot_decode(#payload, &mut #cursor)?)
                }
                NamedAbiKind::Enum => {
                    model.enumeration(named.stable_id).ok_or_else(|| {
                        CodegenError::UnknownNamedType {
                            name: named.source_name.clone(),
                            span: ty.span,
                        }
                    })?;
                    quote!(#ident::__nexa_snapshot_decode(#payload, &mut #cursor)?)
                }
            }
        }
        ResolvedTypeKind::Token(_) | ResolvedTypeKind::Snapshot(_) => {
            quote!(return ::std::result::Result::Err(nexa_runtime::HostTrap::Type))
        }
    })
}
