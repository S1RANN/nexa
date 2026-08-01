use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nexa_bytecode::ValueType;
use nexa_core::{FileId, FingerprintBuilder, SourceSpan, StableId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NidlAst {
    pub source: String,
    pub span: SourceSpan,
    pub contract: ContractDecl,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub handles: Vec<HandleDecl>,
    pub structs: Vec<StructDecl>,
    pub enums: Vec<EnumDecl>,
    pub host: Option<FunctionBlock>,
    pub nexa: Option<FunctionBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionBlock {
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub functions: Vec<FunctionDecl>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub fields: Vec<FieldDecl>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub ty: TypeRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub variants: Vec<VariantDecl>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariantDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub payload: Option<TypeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub is_async: bool,
    pub parameters: Vec<ParameterDecl>,
    pub result: Option<TypeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterDecl {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub ty: TypeRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRef {
    pub kind: TypeKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
    String,
    Array(Box<TypeRef>),
    Buffer(Box<TypeRef>),
    Option(Box<TypeRef>),
    Result(Box<TypeRef>, Box<TypeRef>),
    Token(Box<TypeRef>),
    Snapshot(Box<TypeRef>),
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocComment {
    /// Comment contents with the leading `///` removed.
    pub text: String,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub arguments: Vec<AttributeArgument>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeArgument {
    pub name: Option<String>,
    pub value: AttributeValue,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeValue {
    Identifier(String),
    String(String),
    Integer(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelPolicy {
    ReturnError,
    CancelTask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonPolicy {
    ReturnError,
    Trap,
}

/// A canonical 32-byte Descriptor v2 identity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AbiFingerprint(pub [u8; 32]);

impl AbiFingerprint {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for AbiFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl fmt::Display for AbiFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A Rust identifier which has already passed NIDL naming, keyword, and collision validation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RustName(String);

impl RustName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validated(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl fmt::Display for RustName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NamedAbiKind {
    Handle,
    Struct,
    Enum,
}

/// Complete resolved identity for one named ABI type use.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResolvedNamedType {
    pub source_name: String,
    pub stable_id: StableId,
    pub kind: NamedAbiKind,
    pub rust_name: RustName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedTypeRef {
    pub kind: ResolvedTypeKind,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedTypeKind {
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
    String,
    Array(Box<ResolvedTypeRef>),
    Buffer(Box<ResolvedTypeRef>),
    Option(Box<ResolvedTypeRef>),
    Result(Box<ResolvedTypeRef>, Box<ResolvedTypeRef>),
    Token(ResolvedNamedType),
    Snapshot(ResolvedNamedType),
    Named(ResolvedNamedType),
}

impl ResolvedTypeRef {
    /// Runtime/bytecode ABI lowering is total after NIDL validation.
    #[must_use]
    pub fn value_type(&self) -> ValueType {
        match &self.kind {
            ResolvedTypeKind::I32 => ValueType::I32,
            ResolvedTypeKind::I64 => ValueType::I64,
            ResolvedTypeKind::F32 => ValueType::F32,
            ResolvedTypeKind::F64 => ValueType::F64,
            ResolvedTypeKind::Bool => ValueType::Bool,
            ResolvedTypeKind::Rune => ValueType::Rune,
            ResolvedTypeKind::String => ValueType::String,
            ResolvedTypeKind::Array(inner) => {
                ValueType::Named(nexa_bytecode::array_type(inner.value_type()))
            }
            ResolvedTypeKind::Buffer(inner) => {
                ValueType::Named(nexa_bytecode::buffer_type(inner.value_type()))
            }
            ResolvedTypeKind::Option(inner) => {
                ValueType::Named(nexa_bytecode::option_type(inner.value_type()).type_id)
            }
            ResolvedTypeKind::Result(ok, error) => ValueType::Named(
                nexa_bytecode::result_type(ok.value_type(), error.value_type()).type_id,
            ),
            ResolvedTypeKind::Token(target) => {
                ValueType::Named(nexa_bytecode::resource_token_type(target.stable_id))
            }
            ResolvedTypeKind::Snapshot(target) => {
                ValueType::Named(nexa_bytecode::snapshot_type(target.stable_id))
            }
            ResolvedTypeKind::Named(named) => ValueType::Named(named.stable_id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractRustNames {
    pub contract: RustName,
    pub host_trait: RustName,
    pub nexa_surface: RustName,
    pub host_error: RustName,
    pub host_stub: RustName,
    pub host_registry: RustName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleRustNames {
    pub owned: RustName,
    pub token_wrapper: Option<RustName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRustNames {
    pub wrapper: RustName,
    pub encoder: RustName,
    pub borrowed_ref: RustName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructRustNames {
    pub owned: RustName,
    pub borrowed_ref: RustName,
    pub snapshot: Option<SnapshotRustNames>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumRustNames {
    pub owned: RustName,
    pub borrowed_ref: RustName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FunctionRustNames {
    Host {
        method: RustName,
        completion_ticket: Option<RustName>,
    },
    Nexa {
        marker: RustName,
        args: RustName,
        output: RustName,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedContract {
    pub source: String,
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_names: ContractRustNames,
    pub handles: Vec<ValidatedHandle>,
    pub structs: Vec<ValidatedStruct>,
    pub enums: Vec<ValidatedEnum>,
    pub host_functions: Vec<ValidatedFunction>,
    pub nexa_functions: Vec<ValidatedFunction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedHandle {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_names: HandleRustNames,
    pub declaration_fingerprint: AbiFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedStruct {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_names: StructRustNames,
    pub fields: Vec<ValidatedField>,
    pub declaration_fingerprint: AbiFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedField {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_name: RustName,
    pub ty: ResolvedTypeRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedEnum {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_names: EnumRustNames,
    pub variants: Vec<ValidatedVariant>,
    pub declaration_fingerprint: AbiFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedVariant {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_name: RustName,
    pub payload: Option<ResolvedTypeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedFunction {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_names: FunctionRustNames,
    pub is_async: bool,
    pub parameters: Vec<ValidatedParameter>,
    /// `None` is the NIDL v2 spelling of semantic Unit.
    pub result: Option<ResolvedTypeRef>,
    pub fuel_cost: u32,
    pub cancel_policy: CancelPolicy,
    pub abandon_policy: AbandonPolicy,
    pub capabilities: Vec<String>,
    pub declaration_fingerprint: AbiFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedParameter {
    pub name: String,
    pub name_span: SourceSpan,
    pub span: SourceSpan,
    pub docs: Vec<DocComment>,
    pub attributes: Vec<Attribute>,
    pub stable_id: StableId,
    pub stable_name: Option<String>,
    pub rust_name: RustName,
    pub ty: ResolvedTypeRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NidlErrorKind {
    Syntax,
    Duplicate,
    InvalidName,
    UnknownType,
    InvalidType,
    RecursiveLayout,
    InvalidAttribute,
    RustNameCollision,
    StableIdCollision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NidlError {
    pub kind: NidlErrorKind,
    pub span: SourceSpan,
    pub message: String,
}

impl NidlError {
    #[must_use]
    pub fn new(kind: NidlErrorKind, span: SourceSpan, message: impl Into<String>) -> Self {
        Self {
            kind,
            span,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn syntax(span: SourceSpan, message: impl Into<String>) -> Self {
        Self::new(NidlErrorKind::Syntax, span, message)
    }
}

impl fmt::Display for NidlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at bytes {}..{}",
            self.message, self.span.start, self.span.end
        )
    }
}

impl std::error::Error for NidlError {}

impl ValidatedContract {
    pub fn validate(ast: &NidlAst) -> Result<Self, Vec<NidlError>> {
        Validator::new(ast).validate()
    }
}

struct Validator<'a> {
    ast: &'a NidlAst,
    errors: Vec<NidlError>,
    type_names: BTreeMap<String, SourceSpan>,
    type_symbols: BTreeMap<String, PredeclaredType>,
    handle_names: BTreeSet<String>,
    struct_names: BTreeSet<String>,
    token_targets: BTreeSet<String>,
    snapshot_targets: BTreeSet<String>,
    stable_ids: BTreeMap<StableId, (String, SourceSpan)>,
    rust_names: BTreeMap<String, (String, SourceSpan)>,
}

#[derive(Clone)]
struct PredeclaredType {
    named: ResolvedNamedType,
    stable_name: Option<String>,
}

impl<'a> Validator<'a> {
    fn new(ast: &'a NidlAst) -> Self {
        let (token_targets, snapshot_targets) = generated_wrapper_targets(&ast.contract);
        Self {
            ast,
            errors: Vec::new(),
            type_names: BTreeMap::new(),
            type_symbols: BTreeMap::new(),
            handle_names: ast
                .contract
                .handles
                .iter()
                .map(|handle| handle.name.clone())
                .collect(),
            struct_names: ast
                .contract
                .structs
                .iter()
                .map(|structure| structure.name.clone())
                .collect(),
            token_targets: token_targets.into_keys().collect(),
            snapshot_targets: snapshot_targets.into_keys().collect(),
            stable_ids: BTreeMap::new(),
            rust_names: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn validate(mut self) -> Result<ValidatedContract, Vec<NidlError>> {
        let contract = &self.ast.contract;
        self.require_pascal_case("contract", &contract.name, contract.name_span);
        self.require_not_rust_keyword("contract", &contract.name, contract.name_span);
        let contract_stable_name = self.stable_attribute(&contract.attributes, "contract");
        self.validate_only_stable_attributes(&contract.attributes, "contract");
        for (name, block) in [
            ("host", contract.host.as_ref()),
            ("nexa", contract.nexa.as_ref()),
        ] {
            if let Some(block) = block {
                for attribute in &block.attributes {
                    self.errors.push(NidlError::new(
                        NidlErrorKind::InvalidAttribute,
                        attribute.span,
                        format!("`@{}` is not valid on a `{name}` block", attribute.name),
                    ));
                }
            }
        }
        let contract_stable_id = self.register_stable_id(
            contract_stable_name.as_deref(),
            "contract",
            &[],
            &contract.name,
            format!("contract `{}`", contract.name),
            contract.name_span,
        );

        let contract_rust_names = ContractRustNames {
            contract: RustName::validated(contract.name.clone()),
            host_trait: RustName::validated(format!("{}Host", contract.name)),
            nexa_surface: RustName::validated(format!("{}Nexa", contract.name)),
            host_error: RustName::validated("HostError"),
            host_stub: RustName::validated("GeneratedHostStub"),
            host_registry: RustName::validated("GeneratedHostRegistry"),
        };
        self.register_rust_name(
            contract_rust_names.contract.as_str(),
            format!("contract `{}`", contract.name),
            contract.name_span,
        );
        self.register_rust_name(
            contract_rust_names.host_trait.as_str(),
            "generated Host trait",
            contract.name_span,
        );
        self.register_rust_name(
            contract_rust_names.nexa_surface.as_str(),
            "generated Nexa surface",
            contract.name_span,
        );
        for fixed in [
            &contract_rust_names.host_error,
            &contract_rust_names.host_stub,
            &contract_rust_names.host_registry,
        ] {
            self.register_rust_name(
                fixed.as_str(),
                format!("reserved generated Rust type `{fixed}`"),
                contract.name_span,
            );
        }

        for handle in &contract.handles {
            self.register_type_name("handle", &handle.name, handle.name_span);
        }
        for structure in &contract.structs {
            self.register_type_name("struct", &structure.name, structure.name_span);
        }
        for enumeration in &contract.enums {
            self.register_type_name("enum", &enumeration.name, enumeration.name_span);
        }
        self.predeclare_types(contract_stable_id);
        let (token_targets, snapshot_targets) = generated_wrapper_targets(contract);
        for (name, source) in token_targets {
            if !self.handle_names.contains(&name) {
                continue;
            }
            self.register_rust_name(
                &format!("{name}Token"),
                format!("generated token wrapper for handle `{name}`"),
                source,
            );
        }
        for (name, source) in snapshot_targets {
            if !self.struct_names.contains(&name) {
                continue;
            }
            self.register_rust_name(
                &format!("{name}Snapshot"),
                format!("generated snapshot wrapper for struct `{name}`"),
                source,
            );
            self.register_rust_name(
                &format!("{name}SnapshotEncoder"),
                format!("generated snapshot encoder for struct `{name}`"),
                source,
            );
            self.register_rust_name(
                &format!("{name}SnapshotRef"),
                format!("generated snapshot reference for struct `{name}`"),
                source,
            );
        }

        let handles = contract
            .handles
            .iter()
            .map(|handle| self.validate_handle(handle))
            .collect();
        let structs = contract
            .structs
            .iter()
            .filter_map(|structure| self.validate_struct(structure))
            .collect();
        let enums = contract
            .enums
            .iter()
            .filter_map(|enumeration| self.validate_enum(enumeration))
            .collect();

        let mut host_names = BTreeMap::new();
        let host_functions = contract
            .host
            .iter()
            .flat_map(|block| &block.functions)
            .filter_map(|function| {
                self.register_local_name(
                    &mut host_names,
                    "Host function",
                    &function.name,
                    function.name_span,
                );
                self.validate_function(function, FunctionSide::Host, contract_stable_id)
            })
            .collect();

        let mut nexa_names = BTreeMap::new();
        let nexa_functions = contract
            .nexa
            .iter()
            .flat_map(|block| &block.functions)
            .filter_map(|function| {
                self.register_local_name(
                    &mut nexa_names,
                    "Nexa function",
                    &function.name,
                    function.name_span,
                );
                self.validate_function(function, FunctionSide::Nexa, contract_stable_id)
            })
            .collect();

        self.validate_recursive_layouts();

        let validated = ValidatedContract {
            source: self.ast.source.clone(),
            name: contract.name.clone(),
            name_span: contract.name_span,
            span: contract.span,
            docs: contract.docs.clone(),
            attributes: contract.attributes.clone(),
            stable_id: contract_stable_id,
            stable_name: contract_stable_name,
            rust_names: contract_rust_names,
            handles,
            structs,
            enums,
            host_functions,
            nexa_functions,
        };
        if self.errors.is_empty() {
            Ok(validated)
        } else {
            Err(self.errors)
        }
    }

    fn predeclare_types(&mut self, contract_stable_id: StableId) {
        for handle in &self.ast.contract.handles {
            self.predeclare_type(
                "handle",
                NamedAbiKind::Handle,
                &handle.name,
                handle.name_span,
                &handle.attributes,
                contract_stable_id,
            );
        }
        for structure in &self.ast.contract.structs {
            self.predeclare_type(
                "struct",
                NamedAbiKind::Struct,
                &structure.name,
                structure.name_span,
                &structure.attributes,
                contract_stable_id,
            );
            self.register_rust_name(
                &format!("{}Ref", structure.name),
                format!("generated borrowed view for struct `{}`", structure.name),
                structure.name_span,
            );
        }
        for enumeration in &self.ast.contract.enums {
            self.predeclare_type(
                "enum",
                NamedAbiKind::Enum,
                &enumeration.name,
                enumeration.name_span,
                &enumeration.attributes,
                contract_stable_id,
            );
            self.register_rust_name(
                &format!("{}Ref", enumeration.name),
                format!("generated borrowed view for enum `{}`", enumeration.name),
                enumeration.name_span,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn predeclare_type(
        &mut self,
        category: &str,
        kind: NamedAbiKind,
        name: &str,
        name_span: SourceSpan,
        attributes: &[Attribute],
        contract_stable_id: StableId,
    ) {
        self.validate_only_stable_attributes(attributes, category);
        let stable_name = self.stable_attribute(attributes, category);
        let stable_id = self.register_stable_id(
            stable_name.as_deref(),
            category,
            &[contract_stable_id],
            name,
            format!("{category} `{name}`"),
            name_span,
        );
        self.register_rust_name(name, format!("{category} `{name}`"), name_span);
        self.type_symbols.insert(
            name.to_owned(),
            PredeclaredType {
                named: ResolvedNamedType {
                    source_name: name.to_owned(),
                    stable_id,
                    kind,
                    rust_name: RustName::validated(name),
                },
                stable_name,
            },
        );
    }

    fn validate_handle(&mut self, handle: &HandleDecl) -> ValidatedHandle {
        let symbol = self
            .type_symbols
            .get(&handle.name)
            .expect("every parsed handle was predeclared")
            .clone();
        let rust_names = HandleRustNames {
            owned: symbol.named.rust_name.clone(),
            token_wrapper: self
                .token_targets
                .contains(&handle.name)
                .then(|| RustName::validated(format!("{}Token", handle.name))),
        };
        let declaration_fingerprint =
            crate::descriptor::handle_declaration_fingerprint(symbol.named.stable_id, &handle.name);
        ValidatedHandle {
            name: handle.name.clone(),
            name_span: handle.name_span,
            span: handle.span,
            docs: handle.docs.clone(),
            attributes: handle.attributes.clone(),
            stable_id: symbol.named.stable_id,
            stable_name: symbol.stable_name,
            rust_names,
            declaration_fingerprint,
        }
    }

    fn validate_struct(&mut self, structure: &StructDecl) -> Option<ValidatedStruct> {
        let symbol = self
            .type_symbols
            .get(&structure.name)
            .expect("every parsed struct was predeclared")
            .clone();
        let structure_stable_id = symbol.named.stable_id;
        let mut field_names = BTreeMap::new();
        let resolved_fields = structure
            .fields
            .iter()
            .map(|field| {
                self.register_local_name(&mut field_names, "field", &field.name, field.name_span);
                self.require_snake_case("field", &field.name, field.name_span);
                self.require_not_rust_keyword("field", &field.name, field.name_span);
                let ty = self.resolve_type(&field.ty);
                self.validate_only_stable_attributes(&field.attributes, "field");
                let stable_name = self.stable_attribute(&field.attributes, "field");
                let stable_id = self.register_stable_id(
                    stable_name.as_deref(),
                    "field",
                    &[structure_stable_id],
                    &field.name,
                    format!("field `{}::{}`", structure.name, field.name),
                    field.name_span,
                );
                ty.map(|ty| ValidatedField {
                    name: field.name.clone(),
                    name_span: field.name_span,
                    span: field.span,
                    docs: field.docs.clone(),
                    attributes: field.attributes.clone(),
                    stable_id,
                    stable_name,
                    rust_name: RustName::validated(field.name.clone()),
                    ty,
                })
            })
            .collect::<Vec<_>>();
        if resolved_fields.iter().any(Option::is_none) {
            return None;
        }
        let fields = resolved_fields.into_iter().flatten().collect::<Vec<_>>();
        let rust_names = StructRustNames {
            owned: symbol.named.rust_name,
            borrowed_ref: RustName::validated(format!("{}Ref", structure.name)),
            snapshot: self
                .snapshot_targets
                .contains(&structure.name)
                .then(|| SnapshotRustNames {
                    wrapper: RustName::validated(format!("{}Snapshot", structure.name)),
                    encoder: RustName::validated(format!("{}SnapshotEncoder", structure.name)),
                    borrowed_ref: RustName::validated(format!("{}SnapshotRef", structure.name)),
                }),
        };
        let declaration_fingerprint = crate::descriptor::struct_declaration_fingerprint(
            structure_stable_id,
            &structure.name,
            &fields,
        );

        Some(ValidatedStruct {
            name: structure.name.clone(),
            name_span: structure.name_span,
            span: structure.span,
            docs: structure.docs.clone(),
            attributes: structure.attributes.clone(),
            stable_id: structure_stable_id,
            stable_name: symbol.stable_name,
            rust_names,
            fields,
            declaration_fingerprint,
        })
    }

    fn validate_enum(&mut self, enumeration: &EnumDecl) -> Option<ValidatedEnum> {
        let symbol = self
            .type_symbols
            .get(&enumeration.name)
            .expect("every parsed enum was predeclared")
            .clone();
        let enum_stable_id = symbol.named.stable_id;
        let mut variant_names = BTreeMap::new();
        let resolved_variants = enumeration
            .variants
            .iter()
            .map(|variant| {
                self.register_local_name(
                    &mut variant_names,
                    "variant",
                    &variant.name,
                    variant.name_span,
                );
                self.require_pascal_case("variant", &variant.name, variant.name_span);
                self.require_not_rust_keyword("variant", &variant.name, variant.name_span);
                let payload = match &variant.payload {
                    Some(payload) => self.resolve_type(payload).map(Some),
                    None => Some(None),
                };
                self.validate_only_stable_attributes(&variant.attributes, "variant");
                let stable_name = self.stable_attribute(&variant.attributes, "variant");
                let stable_id = self.register_stable_id(
                    stable_name.as_deref(),
                    "variant",
                    &[enum_stable_id],
                    &variant.name,
                    format!("variant `{}::{}`", enumeration.name, variant.name),
                    variant.name_span,
                );
                payload.map(|payload| ValidatedVariant {
                    name: variant.name.clone(),
                    name_span: variant.name_span,
                    span: variant.span,
                    docs: variant.docs.clone(),
                    attributes: variant.attributes.clone(),
                    stable_id,
                    stable_name,
                    rust_name: RustName::validated(variant.name.clone()),
                    payload,
                })
            })
            .collect::<Vec<_>>();
        if resolved_variants.iter().any(Option::is_none) {
            return None;
        }
        let variants = resolved_variants.into_iter().flatten().collect::<Vec<_>>();
        let rust_names = EnumRustNames {
            owned: symbol.named.rust_name,
            borrowed_ref: RustName::validated(format!("{}Ref", enumeration.name)),
        };
        let declaration_fingerprint = crate::descriptor::enum_declaration_fingerprint(
            enum_stable_id,
            &enumeration.name,
            &variants,
        );

        Some(ValidatedEnum {
            name: enumeration.name.clone(),
            name_span: enumeration.name_span,
            span: enumeration.span,
            docs: enumeration.docs.clone(),
            attributes: enumeration.attributes.clone(),
            stable_id: enum_stable_id,
            stable_name: symbol.stable_name,
            rust_names,
            variants,
            declaration_fingerprint,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn validate_function(
        &mut self,
        function: &FunctionDecl,
        side: FunctionSide,
        contract_stable_id: StableId,
    ) -> Option<ValidatedFunction> {
        self.require_snake_case("function", &function.name, function.name_span);
        self.require_not_rust_keyword("function", &function.name, function.name_span);
        let normalized = self.validate_function_attributes(function, side);
        if side == FunctionSide::Host
            && function.is_async
            && !matches!(
                function.result.as_ref().map(|result| &result.kind),
                Some(TypeKind::Result(_, _))
            )
        {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidType,
                function
                    .result
                    .as_ref()
                    .map_or(function.name_span, |result| result.span),
                "an async Host function must return `Result<T, E>`",
            ));
        }
        if side == FunctionSide::Host
            && function.is_async
            && let Some(TypeKind::Result(_, error)) =
                function.result.as_ref().map(|result| &result.kind)
        {
            if normalized.cancel_policy == CancelPolicy::ReturnError
                && self.error_type_supports(error, "Cancelled") == Some(false)
            {
                self.errors.push(NidlError::new(
                    NidlErrorKind::InvalidType,
                    policy_or_type_span(function, "cancel", error.span),
                    "a Host async `@cancel(return_error)` error must be `i32` or an enum \
                     containing a unit `Cancelled` variant",
                ));
            }
            if normalized.abandon_policy == AbandonPolicy::ReturnError
                && self.error_type_supports(error, "Abandoned") == Some(false)
            {
                self.errors.push(NidlError::new(
                    NidlErrorKind::InvalidType,
                    policy_or_type_span(function, "abandon", error.span),
                    "a Host async `@abandon(return_error)` error must be `i32` or an enum \
                     containing a unit `Abandoned` variant",
                ));
            }
        }
        let side_name = match side {
            FunctionSide::Host => "host",
            FunctionSide::Nexa => "nexa",
        };
        let function_stable_id = self.register_stable_id(
            normalized.stable_name.as_deref(),
            match side {
                FunctionSide::Host => "host-function",
                FunctionSide::Nexa => "nexa-entrypoint",
            },
            &[contract_stable_id],
            &function.name,
            format!("{side_name} function `{}`", function.name),
            function.name_span,
        );

        let rust_base = snake_to_pascal(&function.name);
        match side {
            FunctionSide::Host if function.is_async => self.register_rust_name(
                &format!("{rust_base}CompletionTicket"),
                format!(
                    "generated completion ticket for Host function `{}`",
                    function.name
                ),
                function.name_span,
            ),
            FunctionSide::Host => {}
            FunctionSide::Nexa => {
                self.register_rust_name(
                    &rust_base,
                    format!("generated marker for Nexa function `{}`", function.name),
                    function.name_span,
                );
                self.register_rust_name(
                    &format!("{rust_base}Args"),
                    format!("generated args for Nexa function `{}`", function.name),
                    function.name_span,
                );
                self.register_rust_name(
                    &format!("{rust_base}Output"),
                    format!("generated output for Nexa function `{}`", function.name),
                    function.name_span,
                );
            }
        }
        let rust_names = match side {
            FunctionSide::Host => FunctionRustNames::Host {
                method: RustName::validated(function.name.clone()),
                completion_ticket: function
                    .is_async
                    .then(|| RustName::validated(format!("{rust_base}CompletionTicket"))),
            },
            FunctionSide::Nexa => FunctionRustNames::Nexa {
                marker: RustName::validated(rust_base.clone()),
                args: RustName::validated(format!("{rust_base}Args")),
                output: RustName::validated(format!("{rust_base}Output")),
            },
        };

        let mut parameter_names = BTreeMap::new();
        let resolved_parameters = function
            .parameters
            .iter()
            .map(|parameter| {
                self.register_local_name(
                    &mut parameter_names,
                    "parameter",
                    &parameter.name,
                    parameter.name_span,
                );
                self.require_snake_case("parameter", &parameter.name, parameter.name_span);
                self.require_not_rust_keyword("parameter", &parameter.name, parameter.name_span);
                let ty = self.resolve_type(&parameter.ty);
                self.validate_only_stable_attributes(&parameter.attributes, "parameter");
                let stable_name = self.stable_attribute(&parameter.attributes, "parameter");
                let stable_id = self.register_stable_id(
                    stable_name.as_deref(),
                    "parameter",
                    &[function_stable_id],
                    &parameter.name,
                    format!("parameter `{}::{}`", function.name, parameter.name),
                    parameter.name_span,
                );
                ty.map(|ty| ValidatedParameter {
                    name: parameter.name.clone(),
                    name_span: parameter.name_span,
                    span: parameter.span,
                    docs: parameter.docs.clone(),
                    attributes: parameter.attributes.clone(),
                    stable_id,
                    stable_name,
                    rust_name: RustName::validated(parameter.name.clone()),
                    ty,
                })
            })
            .collect::<Vec<_>>();
        let result = match &function.result {
            Some(result) => self.resolve_type(result).map(Some),
            None => Some(None),
        };
        if resolved_parameters.iter().any(Option::is_none) || result.is_none() {
            return None;
        }
        let parameters = resolved_parameters
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let result = result.expect("successful optional type resolution is present");
        let fingerprint_kind = match side {
            FunctionSide::Host => crate::descriptor::FunctionFingerprintKind::Host,
            FunctionSide::Nexa => crate::descriptor::FunctionFingerprintKind::Nexa,
        };
        let declaration_fingerprint = crate::descriptor::function_declaration_fingerprint(
            &crate::descriptor::FunctionFingerprintInput {
                kind: fingerprint_kind,
                stable_id: function_stable_id,
                name: &function.name,
                is_async: function.is_async,
                parameters: &parameters,
                result: result.as_ref(),
                fuel_cost: normalized.fuel_cost,
                cancel_policy: normalized.cancel_policy,
                abandon_policy: normalized.abandon_policy,
                capabilities: &normalized.capabilities,
            },
        );

        Some(ValidatedFunction {
            name: function.name.clone(),
            name_span: function.name_span,
            span: function.span,
            docs: function.docs.clone(),
            attributes: function.attributes.clone(),
            stable_id: function_stable_id,
            stable_name: normalized.stable_name,
            rust_names,
            is_async: function.is_async,
            parameters,
            result,
            fuel_cost: normalized.fuel_cost,
            cancel_policy: normalized.cancel_policy,
            abandon_policy: normalized.abandon_policy,
            capabilities: normalized.capabilities,
            declaration_fingerprint,
        })
    }

    fn error_type_supports(&self, error: &TypeRef, required_variant: &str) -> Option<bool> {
        match &error.kind {
            TypeKind::I32 => Some(true),
            TypeKind::Named(name) => self
                .ast
                .contract
                .enums
                .iter()
                .find(|enumeration| enumeration.name == *name)
                .map(|enumeration| {
                    enumeration.variants.iter().any(|variant| {
                        variant.name == required_variant && variant.payload.is_none()
                    })
                })
                .or_else(|| self.type_names.contains_key(name).then_some(false)),
            _ => Some(false),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn validate_function_attributes(
        &mut self,
        function: &FunctionDecl,
        side: FunctionSide,
    ) -> NormalizedFunctionAttributes {
        let mut normalized = NormalizedFunctionAttributes::default();
        let mut seen = BTreeSet::new();
        for attribute in &function.attributes {
            if attribute.name != "capability" && !seen.insert(attribute.name.as_str()) {
                self.errors.push(NidlError::new(
                    NidlErrorKind::InvalidAttribute,
                    attribute.span,
                    format!("duplicate `@{}` attribute", attribute.name),
                ));
                continue;
            }
            match attribute.name.as_str() {
                "stable" => {
                    normalized.stable_name = self.string_argument(attribute, "@stable", false);
                }
                "fuel" if side == FunctionSide::Host => {
                    let Some(value) = self.integer_argument(attribute, "@fuel") else {
                        continue;
                    };
                    match u32::try_from(value) {
                        Ok(value) if value > 0 => normalized.fuel_cost = value,
                        _ => self.errors.push(NidlError::new(
                            NidlErrorKind::InvalidAttribute,
                            attribute.span,
                            "@fuel must be an integer in 1..=4294967295",
                        )),
                    }
                }
                "cancel" if side == FunctionSide::Host => {
                    normalized.cancel_policy = match self.identifier_argument(attribute, "@cancel")
                    {
                        Some(value) if value == "return_error" => CancelPolicy::ReturnError,
                        Some(value) if value == "cancel_task" => CancelPolicy::CancelTask,
                        Some(value) => {
                            self.errors.push(NidlError::new(
                                NidlErrorKind::InvalidAttribute,
                                attribute.span,
                                format!(
                                    "unknown @cancel policy `{value}`; expected `return_error` or `cancel_task`"
                                ),
                            ));
                            CancelPolicy::ReturnError
                        }
                        None => CancelPolicy::ReturnError,
                    };
                    if !function.is_async {
                        self.errors.push(NidlError::new(
                            NidlErrorKind::InvalidAttribute,
                            attribute.span,
                            "@cancel is only valid on async Host functions",
                        ));
                    }
                }
                "abandon" if side == FunctionSide::Host => {
                    normalized.abandon_policy = match self
                        .identifier_argument(attribute, "@abandon")
                    {
                        Some(value) if value == "return_error" => AbandonPolicy::ReturnError,
                        Some(value) if value == "trap" => AbandonPolicy::Trap,
                        Some(value) => {
                            self.errors.push(NidlError::new(
                                    NidlErrorKind::InvalidAttribute,
                                    attribute.span,
                                    format!(
                                        "unknown @abandon policy `{value}`; expected `return_error` or `trap`"
                                    ),
                                ));
                            AbandonPolicy::ReturnError
                        }
                        None => AbandonPolicy::ReturnError,
                    };
                    if !function.is_async {
                        self.errors.push(NidlError::new(
                            NidlErrorKind::InvalidAttribute,
                            attribute.span,
                            "@abandon is only valid on async Host functions",
                        ));
                    }
                }
                "capability" if side == FunctionSide::Host => {
                    if let Some(capability) = self.string_argument(attribute, "@capability", false)
                    {
                        if capability.is_empty()
                            || capability.split('.').any(str::is_empty)
                            || !capability.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                            })
                        {
                            self.errors.push(NidlError::new(
                                NidlErrorKind::InvalidAttribute,
                                attribute.span,
                                format!("invalid capability name `{capability}`"),
                            ));
                        } else if normalized.capabilities.contains(&capability) {
                            self.errors.push(NidlError::new(
                                NidlErrorKind::InvalidAttribute,
                                attribute.span,
                                format!("duplicate capability `{capability}`"),
                            ));
                        } else {
                            normalized.capabilities.push(capability);
                        }
                    }
                }
                _ => self.errors.push(NidlError::new(
                    NidlErrorKind::InvalidAttribute,
                    attribute.span,
                    format!(
                        "`@{}` is not valid on a {} function",
                        attribute.name,
                        match side {
                            FunctionSide::Host => "Host",
                            FunctionSide::Nexa => "Nexa",
                        }
                    ),
                )),
            }
        }
        normalized
    }

    fn validate_only_stable_attributes(&mut self, attributes: &[Attribute], target: &str) {
        let mut seen_stable = false;
        for attribute in attributes {
            if attribute.name != "stable" {
                self.errors.push(NidlError::new(
                    NidlErrorKind::InvalidAttribute,
                    attribute.span,
                    format!("`@{}` is not valid on a {target}", attribute.name),
                ));
            } else if seen_stable {
                self.errors.push(NidlError::new(
                    NidlErrorKind::InvalidAttribute,
                    attribute.span,
                    "duplicate `@stable` attribute",
                ));
            }
            seen_stable |= attribute.name == "stable";
        }
    }

    fn stable_attribute(&mut self, attributes: &[Attribute], target: &str) -> Option<String> {
        let mut stable = None;
        for attribute in attributes
            .iter()
            .filter(|attribute| attribute.name == "stable")
        {
            let Some(value) = self.string_argument(attribute, "@stable", false) else {
                continue;
            };
            if value.is_empty()
                || !value.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '_' | '-' | '.' | ':' | '/')
                })
            {
                self.errors.push(NidlError::new(
                    NidlErrorKind::InvalidAttribute,
                    attribute.span,
                    format!("invalid stable name `{value}` for {target}"),
                ));
            } else if stable.is_none() {
                stable = Some(value);
            }
        }
        stable
    }

    fn string_argument(
        &mut self,
        attribute: &Attribute,
        display: &str,
        allow_named: bool,
    ) -> Option<String> {
        let argument = self.single_argument(attribute, display, allow_named)?;
        if let AttributeValue::String(value) = &argument.value {
            Some(value.clone())
        } else {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidAttribute,
                argument.span,
                format!("{display} expects one string argument"),
            ));
            None
        }
    }

    fn integer_argument(&mut self, attribute: &Attribute, display: &str) -> Option<u64> {
        let argument = self.single_argument(attribute, display, false)?;
        if let AttributeValue::Integer(value) = &argument.value {
            Some(*value)
        } else {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidAttribute,
                argument.span,
                format!("{display} expects one integer argument"),
            ));
            None
        }
    }

    fn identifier_argument(&mut self, attribute: &Attribute, display: &str) -> Option<String> {
        let argument = self.single_argument(attribute, display, false)?;
        if let AttributeValue::Identifier(value) = &argument.value {
            Some(value.clone())
        } else {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidAttribute,
                argument.span,
                format!("{display} expects one identifier argument"),
            ));
            None
        }
    }

    fn single_argument<'b>(
        &mut self,
        attribute: &'b Attribute,
        display: &str,
        allow_named: bool,
    ) -> Option<&'b AttributeArgument> {
        if attribute.arguments.len() != 1 {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidAttribute,
                attribute.span,
                format!("{display} expects exactly one argument"),
            ));
            return None;
        }
        let argument = &attribute.arguments[0];
        if !allow_named && argument.name.is_some() {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidAttribute,
                argument.span,
                format!("{display} does not accept named arguments"),
            ));
            return None;
        }
        Some(argument)
    }

    fn resolve_type(&mut self, ty: &TypeRef) -> Option<ResolvedTypeRef> {
        let kind = match &ty.kind {
            TypeKind::I32 => Some(ResolvedTypeKind::I32),
            TypeKind::I64 => Some(ResolvedTypeKind::I64),
            TypeKind::F32 => Some(ResolvedTypeKind::F32),
            TypeKind::F64 => Some(ResolvedTypeKind::F64),
            TypeKind::Bool => Some(ResolvedTypeKind::Bool),
            TypeKind::Rune => Some(ResolvedTypeKind::Rune),
            TypeKind::String => Some(ResolvedTypeKind::String),
            TypeKind::Array(inner) => self
                .resolve_type(inner)
                .map(Box::new)
                .map(ResolvedTypeKind::Array),
            TypeKind::Buffer(inner) => self
                .resolve_type(inner)
                .map(Box::new)
                .map(ResolvedTypeKind::Buffer),
            TypeKind::Option(inner) => self
                .resolve_type(inner)
                .map(Box::new)
                .map(ResolvedTypeKind::Option),
            TypeKind::Result(ok, error) => {
                let ok = self.resolve_type(ok);
                let error = self.resolve_type(error);
                ok.zip(error)
                    .map(|(ok, error)| ResolvedTypeKind::Result(Box::new(ok), Box::new(error)))
            }
            TypeKind::Token(inner) => {
                let inner = self.resolve_type(inner);
                match inner.map(|inner| inner.kind) {
                    Some(ResolvedTypeKind::Named(named)) if named.kind == NamedAbiKind::Handle => {
                        Some(ResolvedTypeKind::Token(named))
                    }
                    Some(_) => {
                        self.errors.push(NidlError::new(
                            NidlErrorKind::InvalidType,
                            ty.span,
                            "`Token<T>` requires `T` to be a declared handle type",
                        ));
                        None
                    }
                    None => None,
                }
            }
            TypeKind::Snapshot(inner) => {
                let inner = self.resolve_type(inner);
                match inner.map(|inner| inner.kind) {
                    Some(ResolvedTypeKind::Named(named)) if named.kind == NamedAbiKind::Struct => {
                        Some(ResolvedTypeKind::Snapshot(named))
                    }
                    Some(_) => {
                        self.errors.push(NidlError::new(
                            NidlErrorKind::InvalidType,
                            ty.span,
                            "`Snapshot<T>` requires `T` to be a declared struct type",
                        ));
                        None
                    }
                    None => None,
                }
            }
            TypeKind::Named(name) => {
                if let Some(symbol) = self.type_symbols.get(name) {
                    Some(ResolvedTypeKind::Named(symbol.named.clone()))
                } else {
                    self.errors.push(NidlError::new(
                        NidlErrorKind::UnknownType,
                        ty.span,
                        format!("unknown NIDL type `{name}`"),
                    ));
                    None
                }
            }
        };
        kind.map(|kind| ResolvedTypeRef {
            kind,
            span: ty.span,
        })
    }

    fn validate_recursive_layouts(&mut self) {
        let handle_names: BTreeSet<_> = self
            .ast
            .contract
            .handles
            .iter()
            .map(|handle| handle.name.as_str())
            .collect();
        let mut graph: BTreeMap<&str, BTreeMap<&str, SourceSpan>> = BTreeMap::new();
        for structure in &self.ast.contract.structs {
            let edges = graph.entry(&structure.name).or_default();
            for field in &structure.fields {
                collect_layout_edges(&field.ty, &handle_names, edges);
            }
        }
        for enumeration in &self.ast.contract.enums {
            let edges = graph.entry(&enumeration.name).or_default();
            for variant in &enumeration.variants {
                if let Some(payload) = &variant.payload {
                    collect_layout_edges(payload, &handle_names, edges);
                }
            }
        }

        let mut complete = BTreeSet::new();
        let mut active = Vec::new();
        let mut reported = BTreeSet::new();
        for name in graph.keys().copied().collect::<Vec<_>>() {
            visit_layout(
                name,
                &graph,
                &mut active,
                &mut complete,
                &mut reported,
                &mut self.errors,
                &self.type_names,
                None,
            );
        }
    }

    fn register_type_name(&mut self, kind: &str, name: &str, span: SourceSpan) {
        self.require_pascal_case(kind, name, span);
        self.require_not_rust_keyword(kind, name, span);
        if matches!(
            name,
            "Array" | "Buffer" | "Option" | "Result" | "Token" | "Snapshot"
        ) {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidName,
                span,
                format!("{kind} name `{name}` collides with a built-in NIDL type constructor"),
            ));
        }
        if let Some(previous) = self.type_names.insert(name.to_owned(), span) {
            self.errors.push(NidlError::new(
                NidlErrorKind::Duplicate,
                span,
                format!(
                    "duplicate type name `{name}`; first declaration is at bytes {}..{}",
                    previous.start, previous.end
                ),
            ));
        }
    }

    fn register_local_name(
        &mut self,
        names: &mut BTreeMap<String, SourceSpan>,
        kind: &str,
        name: &str,
        span: SourceSpan,
    ) {
        if let Some(previous) = names.insert(name.to_owned(), span) {
            self.errors.push(NidlError::new(
                NidlErrorKind::Duplicate,
                span,
                format!(
                    "duplicate {kind} name `{name}`; first declaration is at bytes {}..{}",
                    previous.start, previous.end
                ),
            ));
        }
    }

    fn register_stable_id(
        &mut self,
        explicit: Option<&str>,
        category: &str,
        parent_ids: &[StableId],
        source_name: &str,
        owner: impl Into<String>,
        span: SourceSpan,
    ) -> StableId {
        let owner = owner.into();
        let stable_id = declaration_stable_id(explicit, category, parent_ids, source_name);
        if let Some((previous_owner, previous_span)) =
            self.stable_ids.insert(stable_id, (owner.clone(), span))
        {
            self.errors.push(NidlError::new(
                NidlErrorKind::StableIdCollision,
                span,
                format!(
                    "stable ID collision between {owner} and {previous_owner} at bytes {}..{}",
                    previous_span.start, previous_span.end
                ),
            ));
        }
        stable_id
    }

    fn register_rust_name(&mut self, name: &str, owner: impl Into<String>, span: SourceSpan) {
        let owner = owner.into();
        if let Some((previous_owner, previous_span)) = self
            .rust_names
            .insert(name.to_owned(), (owner.clone(), span))
        {
            self.errors.push(NidlError::new(
                NidlErrorKind::RustNameCollision,
                span,
                format!(
                    "Rust name `{name}` collides between {owner} and {previous_owner} at bytes {}..{}",
                    previous_span.start, previous_span.end
                ),
            ));
        }
    }

    fn require_pascal_case(&mut self, kind: &str, name: &str, span: SourceSpan) {
        if !is_pascal_case(name) {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidName,
                span,
                format!("{kind} name `{name}` must be PascalCase"),
            ));
        }
    }

    fn require_snake_case(&mut self, kind: &str, name: &str, span: SourceSpan) {
        if !is_snake_case(name) {
            self.errors.push(NidlError::new(
                NidlErrorKind::InvalidName,
                span,
                format!("{kind} name `{name}` must be snake_case"),
            ));
        }
    }

    fn require_not_rust_keyword(&mut self, kind: &str, name: &str, span: SourceSpan) {
        if is_rust_keyword(name) {
            self.errors.push(NidlError::new(
                NidlErrorKind::RustNameCollision,
                span,
                format!("{kind} name `{name}` collides with a Rust keyword"),
            ));
        }
    }
}

fn declaration_stable_id(
    explicit: Option<&str>,
    category: &str,
    parent_ids: &[StableId],
    source_name: &str,
) -> StableId {
    let mut builder = FingerprintBuilder::new("nexa.nidl.stable-id", 2);
    builder.field_str("category", category);
    builder.field_u32(
        "parent-count",
        u32::try_from(parent_ids.len()).expect("a declaration has a fixed small parent depth"),
    );
    for parent in parent_ids {
        builder.field_u64("parent", parent.0);
    }
    if let Some(stable_name) = explicit {
        builder.field_str("stable", stable_name);
    } else {
        builder.field_str("name", source_name);
    }
    let digest = builder.finish_bytes();
    StableId(u64::from_le_bytes(
        digest[..8]
            .try_into()
            .expect("a BLAKE3 fingerprint has eight leading bytes"),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FunctionSide {
    Host,
    Nexa,
}

#[derive(Debug)]
struct NormalizedFunctionAttributes {
    stable_name: Option<String>,
    fuel_cost: u32,
    cancel_policy: CancelPolicy,
    abandon_policy: AbandonPolicy,
    capabilities: Vec<String>,
}

impl Default for NormalizedFunctionAttributes {
    fn default() -> Self {
        Self {
            stable_name: None,
            fuel_cost: 1,
            cancel_policy: CancelPolicy::ReturnError,
            abandon_policy: AbandonPolicy::ReturnError,
            capabilities: Vec::new(),
        }
    }
}

fn generated_wrapper_targets(
    contract: &ContractDecl,
) -> (BTreeMap<String, SourceSpan>, BTreeMap<String, SourceSpan>) {
    let mut tokens = BTreeMap::new();
    let mut snapshots = BTreeMap::new();
    for structure in &contract.structs {
        for field in &structure.fields {
            collect_generated_wrappers(&field.ty, &mut tokens, &mut snapshots);
        }
    }
    for enumeration in &contract.enums {
        for variant in &enumeration.variants {
            if let Some(payload) = &variant.payload {
                collect_generated_wrappers(payload, &mut tokens, &mut snapshots);
            }
        }
    }
    for function in contract
        .host
        .iter()
        .chain(contract.nexa.iter())
        .flat_map(|block| &block.functions)
    {
        for parameter in &function.parameters {
            collect_generated_wrappers(&parameter.ty, &mut tokens, &mut snapshots);
        }
        if let Some(result) = &function.result {
            collect_generated_wrappers(result, &mut tokens, &mut snapshots);
        }
    }
    (tokens, snapshots)
}

fn policy_or_type_span(function: &FunctionDecl, policy: &str, fallback: SourceSpan) -> SourceSpan {
    function
        .attributes
        .iter()
        .find(|attribute| attribute.name == policy)
        .map_or(fallback, |attribute| attribute.span)
}

fn collect_generated_wrappers(
    ty: &TypeRef,
    tokens: &mut BTreeMap<String, SourceSpan>,
    snapshots: &mut BTreeMap<String, SourceSpan>,
) {
    match &ty.kind {
        TypeKind::Token(inner) => {
            if let TypeKind::Named(name) = &inner.kind {
                tokens.entry(name.clone()).or_insert(inner.span);
            }
        }
        TypeKind::Snapshot(inner) => {
            if let TypeKind::Named(name) = &inner.kind {
                snapshots.entry(name.clone()).or_insert(inner.span);
            }
        }
        TypeKind::Array(inner) | TypeKind::Buffer(inner) | TypeKind::Option(inner) => {
            collect_generated_wrappers(inner, tokens, snapshots);
        }
        TypeKind::Result(ok, error) => {
            collect_generated_wrappers(ok, tokens, snapshots);
            collect_generated_wrappers(error, tokens, snapshots);
        }
        TypeKind::I32
        | TypeKind::I64
        | TypeKind::F32
        | TypeKind::F64
        | TypeKind::Bool
        | TypeKind::Rune
        | TypeKind::String
        | TypeKind::Named(_) => {}
    }
}

fn collect_layout_edges<'a>(
    ty: &'a TypeRef,
    handles: &BTreeSet<&str>,
    edges: &mut BTreeMap<&'a str, SourceSpan>,
) {
    match &ty.kind {
        TypeKind::Named(name) if !handles.contains(name.as_str()) => {
            edges.entry(name).or_insert(ty.span);
        }
        TypeKind::Option(inner) => collect_layout_edges(inner, handles, edges),
        TypeKind::Result(ok, error) => {
            collect_layout_edges(ok, handles, edges);
            collect_layout_edges(error, handles, edges);
        }
        TypeKind::Array(_)
        | TypeKind::Buffer(_)
        | TypeKind::Token(_)
        | TypeKind::Snapshot(_)
        | TypeKind::I32
        | TypeKind::I64
        | TypeKind::F32
        | TypeKind::F64
        | TypeKind::Bool
        | TypeKind::Rune
        | TypeKind::String
        | TypeKind::Named(_) => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn visit_layout<'a>(
    name: &'a str,
    graph: &BTreeMap<&'a str, BTreeMap<&'a str, SourceSpan>>,
    active: &mut Vec<&'a str>,
    complete: &mut BTreeSet<&'a str>,
    reported: &mut BTreeSet<Vec<&'a str>>,
    errors: &mut Vec<NidlError>,
    spans: &BTreeMap<String, SourceSpan>,
    incoming_span: Option<SourceSpan>,
) {
    if complete.contains(name) {
        return;
    }
    if let Some(start) = active.iter().position(|candidate| *candidate == name) {
        let mut cycle = active[start..].to_vec();
        cycle.push(name);
        if reported.insert(cycle.clone()) {
            let span = incoming_span
                .or_else(|| spans.get(name).copied())
                .unwrap_or_default();
            errors.push(NidlError::new(
                NidlErrorKind::RecursiveLayout,
                span,
                format!(
                    "recursive value layout is forbidden: {}",
                    cycle.join(" -> ")
                ),
            ));
        }
        return;
    }
    let Some(edges) = graph.get(name) else {
        return;
    };
    active.push(name);
    for (edge, edge_span) in edges {
        if graph.contains_key(edge) {
            visit_layout(
                edge,
                graph,
                active,
                complete,
                reported,
                errors,
                spans,
                Some(*edge_span),
            );
        }
    }
    active.pop();
    complete.insert(name);
}

fn is_pascal_case(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_uppercase())
        && characters.all(|character| character.is_ascii_alphanumeric())
}

fn is_snake_case(name: &str) -> bool {
    if name.is_empty() || name.starts_with('_') || name.ends_with('_') || name.contains("__") {
        return false;
    }
    name.split('_').all(|segment| {
        let mut characters = segment.chars();
        characters
            .next()
            .is_some_and(|character| character.is_ascii_lowercase())
            && characters
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    })
}

fn snake_to_pascal(name: &str) -> String {
    let mut output = String::with_capacity(name.len());
    for segment in name.split('_') {
        let mut characters = segment.chars();
        if let Some(first) = characters.next() {
            output.push(first.to_ascii_uppercase());
            output.extend(characters);
        }
    }
    output
}

fn is_rust_keyword(name: &str) -> bool {
    matches!(
        name,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "try"
            | "gen"
    )
}

#[must_use]
pub const fn empty_span(file: FileId) -> SourceSpan {
    SourceSpan::new(file, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::{NidlErrorKind, ValidatedContract};

    fn validation_errors(source: &str) -> Vec<super::NidlError> {
        let ast = crate::parser::parse(source).expect("test source has valid NIDL syntax");
        ValidatedContract::validate(&ast).expect_err("test source must fail semantic validation")
    }

    #[test]
    fn validates_typed_handle_targets_and_async_host_results() {
        for source in [
            "contract Bad { struct Value {} host { fn token() -> Token<Value>; } }",
            "contract Bad { handle Entity; host { fn snapshot() -> Snapshot<Entity>; } }",
            "contract Bad { host { async fn load() -> i32; } }",
        ] {
            assert!(
                validation_errors(source)
                    .iter()
                    .any(|error| error.kind == NidlErrorKind::InvalidType),
                "{source}"
            );
        }

        let ast =
            crate::parser::parse("contract Good { nexa { async fn update() -> i32; } }").unwrap();
        ValidatedContract::validate(&ast).expect("async Nexa results are unrestricted");
    }

    #[test]
    fn rejects_reserved_generated_rust_names() {
        for source in [
            "contract Bad { struct HostError {} }",
            "contract Bad { handle Job; struct JobToken {} host { fn token() -> Token<Job>; } }",
            "contract Bad { struct Record {} struct RecordSnapshot {} host { fn snapshot() -> Snapshot<Record>; } }",
        ] {
            assert!(
                validation_errors(source)
                    .iter()
                    .any(|error| error.kind == NidlErrorKind::RustNameCollision),
                "{source}"
            );
        }
    }
}
