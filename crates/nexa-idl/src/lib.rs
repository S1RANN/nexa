//! NIDL v2 syntax lowering, validation, ABI descriptors, and structured Rust bindings.
//!
//! The crate has one front end:
//! `nexa-syntax::parse_nidl` → [`NidlAst`] → [`ValidatedContract`].
//! Rust bindings are built as token trees and are parsed twice before source is returned.

pub mod build;
pub mod codegen;
pub mod descriptor;
pub mod model;
pub mod parser;

use nexa_bytecode::ValueType;
use nexa_core::{FileId, StableId};

pub use codegen::{BindingModel, CodegenError, generate_rust, generate_rust_tokens};
pub use descriptor::{
    ABI_DESCRIPTOR_VERSION, AbiDescriptor, DeclarationFingerprint, EffectiveContractDescriptor,
    EffectiveContractSelection, EffectiveDescriptorError, NIDL_SYNTAX_VERSION, abi_descriptor,
    contract_fingerprint, effective_contract_descriptor, effective_contract_fingerprint,
    host_function_fingerprints, nexa_entrypoint_fingerprints, type_layout_fingerprints,
};
pub use model::{
    AbandonPolicy, AbiFingerprint, Attribute, AttributeArgument, AttributeValue, CancelPolicy,
    ContractDecl, ContractRustNames, DocComment, EnumDecl, EnumRustNames, FieldDecl, FunctionBlock,
    FunctionDecl, FunctionRustNames, HandleDecl, HandleRustNames, NamedAbiKind, NidlAst, NidlError,
    NidlErrorKind, ParameterDecl, ResolvedNamedType, ResolvedTypeKind, ResolvedTypeRef, RustName,
    SnapshotRustNames, StructDecl, StructRustNames, TypeKind, TypeRef, ValidatedContract,
    ValidatedEnum, ValidatedField, ValidatedFunction, ValidatedHandle, ValidatedParameter,
    ValidatedStruct, ValidatedVariant, VariantDecl,
};

/// Lowers one exact NIDL v2 source snapshot into an AST with source spans and documentation.
pub fn parse_ast(source: &str) -> Result<NidlAst, NidlError> {
    parse_ast_with_file_id(source, FileId(0))
}

/// Like [`parse_ast`], assigning every returned span to `file`.
pub fn parse_ast_with_file_id(source: &str, file: FileId) -> Result<NidlAst, NidlError> {
    parser::parse_with_file_id(source, file).map_err(first_error)
}

/// Performs all NIDL name, type, layout, attribute, and generated-name validation.
pub fn validate(ast: &NidlAst) -> Result<ValidatedContract, NidlError> {
    ValidatedContract::validate(ast).map_err(first_error)
}

/// Parses and validates one NIDL v2 Contract.
pub fn parse(source: &str) -> Result<ValidatedContract, NidlError> {
    parse_with_file_id(source, FileId(0))
}

/// Like [`parse`], assigning every returned span to `file`.
pub fn parse_with_file_id(source: &str, file: FileId) -> Result<ValidatedContract, NidlError> {
    let ast = parse_ast_with_file_id(source, file)?;
    validate(&ast)
}

fn first_error(mut errors: Vec<NidlError>) -> NidlError {
    if errors.is_empty() {
        return NidlError::syntax(
            nexa_core::SourceSpan::new(FileId(0), 0, 0),
            "NIDL validation failed without a diagnostic",
        );
    }
    errors.remove(0)
}

/// Runtime bridge for the full 32-byte Contract ABI fingerprint.
///
/// Runtime bytecode currently stores a compact [`StableId`]. It is always the first eight
/// little-endian bytes of [`contract_fingerprint`], never the Contract name's symbol ID.
#[must_use]
pub fn contract_runtime_id(contract: &ValidatedContract) -> StableId {
    let fingerprint = contract_fingerprint(contract);
    StableId(u64::from_le_bytes(
        fingerprint.0[..8]
            .try_into()
            .expect("an ABI fingerprint contains 32 bytes"),
    ))
}

/// Stable source identity of a declared Nexa entrypoint.
#[must_use]
pub const fn entrypoint_stable_id(function: &ValidatedFunction) -> StableId {
    function.stable_id
}

/// Bytecode value signature of a declared Nexa entrypoint.
#[must_use]
pub fn entrypoint_signature(function: &ValidatedFunction) -> nexa_bytecode::Signature {
    nexa_bytecode::Signature {
        parameters: function
            .parameters
            .iter()
            .map(|parameter| abi_value_type(&parameter.ty))
            .collect(),
        result: function.result.as_ref().map(abi_value_type),
    }
}

/// Bytecode value signature of a declared Host function.
#[must_use]
pub fn host_function_signature(function: &ValidatedFunction) -> nexa_bytecode::Signature {
    entrypoint_signature(function)
}

/// Lowers one validated NIDL type to its bytecode ABI value identity.
#[must_use]
pub fn abi_value_type(ty: &ResolvedTypeRef) -> ValueType {
    ty.value_type()
}

/// Canonical identity for a required entrypoint set. Input order has no semantic meaning.
#[must_use]
pub fn required_entrypoints_descriptor<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let mut names = names.into_iter().collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"nexa.required-entrypoints");
    bytes.extend_from_slice(&ABI_DESCRIPTOR_VERSION.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(names.len()).unwrap_or(u32::MAX).to_le_bytes());
    for name in names {
        bytes.extend_from_slice(&u32::try_from(name.len()).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
    }
    bytes
}
