//! Exact-build IDL parsing, canonical hashing and Rust binding generation.

use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write;

use nexa_bytecode::ValueType;
use nexa_core::StableId;

pub mod build;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Idl {
    pub interface: String,
    pub opaque_handles: Vec<String>,
    pub structs: Vec<Struct>,
    pub enums: Vec<Enum>,
    pub functions: Vec<HostFunction>,
    pub exports: Vec<Export>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enum {
    pub name: String,
    pub variants: Vec<EnumVariant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Option<TypeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Struct {
    pub name: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFunction {
    pub name: String,
    pub parameters: Vec<Field>,
    pub result: TypeRef,
    pub synchronous: bool,
    pub fuel_cost: u32,
    pub cancel_policy: CancelPolicy,
    pub abandon_policy: AbandonPolicy,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub parameters: Vec<Field>,
    pub result: Option<TypeRef>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeRef {
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
    String,
    HostRequest(Option<Box<TypeRef>>),
    ResourceToken(Option<Box<TypeRef>>),
    Snapshot(Option<Box<TypeRef>>),
    Array(Box<TypeRef>),
    Buffer(Box<TypeRef>),
    Option(Box<TypeRef>),
    Result(Box<TypeRef>, Box<TypeRef>),
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdlError {
    MissingInterface,
    Syntax(String),
    Duplicate(String),
    UnknownType(String),
}

impl fmt::Display for IdlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for IdlError {}

pub fn parse(source: &str) -> Result<Idl, IdlError> {
    let cleaned = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ");
    let tokens = tokenize(&cleaned);
    let mut parser = Parser { tokens, cursor: 0 };
    parser.parse()
}

#[must_use]
pub fn exact_hash(idl: &Idl) -> StableId {
    StableId::from_name(&canonical(idl))
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn canonical(idl: &Idl) -> String {
    let mut output = format!("interface:{};", idl.interface);
    for handle in &idl.opaque_handles {
        write!(
            output,
            "opaque:{handle}#{:016x};",
            StableId::from_name(handle).0
        )
        .expect("String writes do not fail");
    }
    for structure in &idl.structs {
        write!(
            output,
            "struct:{}#{:016x}{{",
            structure.name,
            StableId::from_name(&structure.name).0
        )
        .expect("String writes do not fail");
        for field in &structure.fields {
            write!(
                output,
                "{}#{:016x}:{};",
                field.name,
                StableId::from_parts(&[&structure.name, "::", &field.name]).0,
                abi_type_descriptor(idl, &field.ty)
            )
            .expect("String writes do not fail");
        }
        output.push('}');
    }
    for enumeration in &idl.enums {
        write!(
            output,
            "enum:{}#{:016x}{{",
            enumeration.name,
            StableId::from_name(&enumeration.name).0
        )
        .expect("String writes do not fail");
        for variant in &enumeration.variants {
            write!(
                output,
                "{}#{:016x}",
                variant.name,
                StableId::from_parts(&[&enumeration.name, "::", &variant.name]).0
            )
            .expect("String writes do not fail");
            if let Some(payload) = &variant.payload {
                write!(output, "({})", abi_type_descriptor(idl, payload))
                    .expect("String writes do not fail");
            }
            output.push(';');
        }
        output.push('}');
    }
    for function in &idl.functions {
        write!(
            output,
            "fn:{}#{:016x}:{}:fuel={}:{}:{}(",
            function.name,
            StableId::from_parts(&[&idl.interface, "::", &function.name]).0,
            if function.synchronous {
                "sync"
            } else {
                "request"
            },
            function.fuel_cost,
            match function.cancel_policy {
                CancelPolicy::ReturnError => "return_error",
                CancelPolicy::CancelTask => "cancel_task",
            },
            match function.abandon_policy {
                AbandonPolicy::ReturnError => "return_error",
                AbandonPolicy::Trap => "trap",
            },
        )
        .expect("String writes do not fail");
        for parameter in &function.parameters {
            write!(
                output,
                "{}:{};",
                parameter.name,
                abi_type_descriptor(idl, &parameter.ty)
            )
            .expect("String writes do not fail");
        }
        write!(output, ")->{};", abi_type_descriptor(idl, &function.result))
            .expect("String writes do not fail");
    }
    for export in &idl.exports {
        write!(output, "export:{}(", export.name).expect("String writes do not fail");
        for parameter in &export.parameters {
            write!(
                output,
                "{}:{};",
                parameter.name,
                abi_type_descriptor(idl, &parameter.ty)
            )
            .expect("String writes do not fail");
        }
        if let Some(result) = &export.result {
            write!(output, ")->{};", abi_type_descriptor(idl, result))
                .expect("String writes do not fail");
        } else {
            output.push_str(")->void;");
        }
    }
    output
}

#[must_use]
pub fn canonical_source(idl: &Idl) -> String {
    let mut output = format!("interface {} {{\n", idl.interface);
    for handle in &idl.opaque_handles {
        writeln!(output, "    opaque {handle};").expect("String writes do not fail");
    }
    for structure in &idl.structs {
        write!(output, "    struct {} {{ ", structure.name).expect("String writes do not fail");
        for field in &structure.fields {
            write!(output, "{}: {}; ", field.name, type_name(&field.ty))
                .expect("String writes do not fail");
        }
        output.push_str("}\n");
    }
    for enumeration in &idl.enums {
        write!(output, "    enum {} {{ ", enumeration.name).expect("String writes do not fail");
        for variant in &enumeration.variants {
            write!(output, "{}", variant.name).expect("String writes do not fail");
            if let Some(payload) = &variant.payload {
                write!(output, "({})", type_name(payload)).expect("String writes do not fail");
            }
            output.push_str(", ");
        }
        output.push_str("}\n");
    }
    for function in &idl.functions {
        if function.synchronous {
            output.push_str("    sync ");
        } else {
            write!(
                output,
                "    request({}, {}) ",
                match function.cancel_policy {
                    CancelPolicy::ReturnError => "return_error",
                    CancelPolicy::CancelTask => "cancel_task",
                },
                match function.abandon_policy {
                    AbandonPolicy::ReturnError => "return_error",
                    AbandonPolicy::Trap => "trap",
                },
            )
            .expect("String writes do not fail");
        }
        if function.fuel_cost != 1 {
            write!(output, "fuel {} ", function.fuel_cost).expect("String writes do not fail");
        }
        write!(output, "fn {}(", function.name).expect("String writes do not fail");
        for (index, parameter) in function.parameters.iter().enumerate() {
            if index != 0 {
                output.push_str(", ");
            }
            write!(output, "{}: {}", parameter.name, type_name(&parameter.ty))
                .expect("String writes do not fail");
        }
        writeln!(output, ") -> {};", type_name(&function.result))
            .expect("String writes do not fail");
    }
    for export in &idl.exports {
        write!(output, "    export {}(", export.name).expect("String writes do not fail");
        for (index, parameter) in export.parameters.iter().enumerate() {
            if index != 0 {
                output.push_str(", ");
            }
            write!(output, "{}: {}", parameter.name, type_name(&parameter.ty))
                .expect("String writes do not fail");
        }
        writeln!(
            output,
            ") -> {};",
            export.result.as_ref().map_or("void".to_owned(), type_name)
        )
        .expect("String writes do not fail");
    }
    output.push_str("}\n");
    output
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn generate_rust(idl: &Idl) -> String {
    let mut output = String::from(
        "// @generated by nexa-idl; do not edit.\n\
         #[derive(Debug)] pub struct HostError(pub String);\n",
    );
    writeln!(
        output,
        "pub const INTERFACE_HASH: nexa_runtime::StableId = nexa_runtime::StableId({});",
        exact_hash(idl).0
    )
    .expect("String writes do not fail");
    writeln!(
        output,
        "pub const NEXA_MODULE_DECLARATION: &str = {:?};",
        canonical(idl)
    )
    .expect("String writes do not fail");
    writeln!(
        output,
        "pub const CANONICAL_IDL: &str = {:?};",
        canonical_source(idl)
    )
    .expect("String writes do not fail");
    output.push_str(
        "pub fn contract() -> nexa_runtime::HostContract {\n\
         nexa_runtime::HostContract {\n",
    );
    writeln!(
        output,
        "interface_name: {:?}, canonical_idl: CANONICAL_IDL, interface_hash: INTERFACE_HASH, \
         generator_schema_version: nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION,\n}} }}",
        idl.interface
    )
    .expect("String writes do not fail");
    for handle in &idl.opaque_handles {
        writeln!(
            output,
            "#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] pub struct {handle}(pub u64);"
        )
        .expect("String writes do not fail");
    }
    let (token_domains, snapshot_contents) = collect_typed_handles(idl);
    for domain in token_domains {
        writeln!(
            output,
            "#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)] \
             pub struct {domain}Token(nexa_runtime::ResourceTokenHandle);\n\
             impl {domain}Token {{\n\
             pub const fn from_raw(handle: nexa_runtime::ResourceTokenHandle) -> Self {{ \
             Self(handle) }}\n\
             pub const fn into_raw(self) -> nexa_runtime::ResourceTokenHandle {{ self.0 }}\n\
             }}\n\
             impl From<{domain}Token> for nexa_runtime::ResourceTokenHandle {{ \
             fn from(value: {domain}Token) -> Self {{ value.into_raw() }} }}\n\
             impl From<nexa_runtime::ResourceTokenHandle> for {domain}Token {{ \
             fn from(value: nexa_runtime::ResourceTokenHandle) -> Self {{ Self::from_raw(value) }} \
             }}"
        )
        .expect("String writes do not fail");
    }
    for content in snapshot_contents {
        let content_type = StableId::from_name(&content);
        let snapshot_type = nexa_bytecode::snapshot_type(content_type).0;
        writeln!(
            output,
            "#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)] \
             pub struct {content}Snapshot(nexa_runtime::SnapshotHandle);\n\
             impl {content}Snapshot {{\n\
             pub const TYPE_ID: nexa_runtime::StableId = nexa_runtime::StableId({snapshot_type});\n\
             pub fn try_from_raw(handle: nexa_runtime::SnapshotHandle) \
             -> Result<Self, nexa_runtime::HostTrap> {{ if handle.type_id() == Self::TYPE_ID {{ \
             Ok(Self(handle)) }} else {{ Err(nexa_runtime::HostTrap::Type) }} }}\n\
             pub const fn into_raw(self) -> nexa_runtime::SnapshotHandle {{ self.0 }}\n\
             }}\n\
             impl TryFrom<nexa_runtime::SnapshotHandle> for {content}Snapshot {{ \
             type Error = nexa_runtime::HostTrap; \
             fn try_from(value: nexa_runtime::SnapshotHandle) -> Result<Self, Self::Error> {{ \
             Self::try_from_raw(value) }} }}\n\
             impl From<{content}Snapshot> for nexa_runtime::SnapshotHandle {{ \
             fn from(value: {content}Snapshot) -> Self {{ value.into_raw() }} }}"
        )
        .expect("String writes do not fail");
        if let Some(structure) = idl.structs.iter().find(|item| item.name == content) {
            let schema_hash = StableId::from_name(&format!("typed-snapshot:{structure:?}"));
            let encoded = structure
                .fields
                .iter()
                .map(|field| {
                    snapshot_encode_statements(
                        idl,
                        &field.ty,
                        &format!("&value.{}", field.name),
                        "__nexa_bytes",
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let decoded = structure
                .fields
                .iter()
                .map(|field| {
                    format!(
                        "{}: {},",
                        field.name,
                        snapshot_decode_expression(
                            idl,
                            &field.ty,
                            "__nexa_payload",
                            "__nexa_cursor"
                        )
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            writeln!(
                output,
                "pub struct {content}SnapshotEncoder;\n\
                 impl {content}SnapshotEncoder {{\n\
                 pub const CONTENT_TYPE: nexa_runtime::StableId = \
                 nexa_runtime::StableId({content_id});\n\
                 pub const SCHEMA_HASH: nexa_runtime::StableId = \
                 nexa_runtime::StableId({schema_id});\n\
                 #[allow(clippy::deref_addrof)] \
                 pub fn encode(value: &{content}) -> Result<nexa_runtime::EncodedSnapshot, \
                 HostError> {{ let mut __nexa_bytes = Vec::new(); {encoded} \
                 nexa_runtime::EncodedSnapshot::new(Self::CONTENT_TYPE, Self::SCHEMA_HASH, 1, \
                 std::sync::Arc::from(__nexa_bytes)).map_err(|error| \
                 HostError(format!(\"{{error:?}}\"))) }} }}\n\
                 #[derive(Clone, Copy, Debug)] pub struct {content}SnapshotRef<'a>(\
                 nexa_runtime::TypedSnapshotRef<'a>);\n\
                 impl<'a> nexa_runtime::DecodeTypedSnapshot<'a> for \
                 {content}SnapshotRef<'a> {{\n\
                 const TYPE_ID: nexa_runtime::StableId = nexa_runtime::StableId({snapshot_type});\n\
                 const CONTENT_TYPE: nexa_runtime::StableId = \
                 nexa_runtime::StableId({content_id});\n\
                 const SCHEMA_HASH: nexa_runtime::StableId = \
                 nexa_runtime::StableId({schema_id});\n\
                 const ALIGNMENT: u16 = 1;\n\
                 fn decode(view: nexa_runtime::TypedSnapshotRef<'a>) -> \
                 Result<Self, nexa_runtime::HostTrap> {{ Ok(Self(view)) }} }}\n\
                 impl<'a> {content}SnapshotRef<'a> {{\n\
                 pub fn decode_owned(self) -> Result<{content}, nexa_runtime::HostTrap> {{ \
                 let __nexa_payload = self.0.payload(); let mut __nexa_cursor = 0usize; \
                 let value = {content} {{ {decoded} }}; \
                 if __nexa_cursor != __nexa_payload.len() {{ \
                 return Err(nexa_runtime::HostTrap::Type); }} Ok(value) }} }}",
                content_id = content_type.0,
                schema_id = schema_hash.0,
            )
            .expect("String writes do not fail");
        }
    }
    for enumeration in &idl.enums {
        writeln!(
            output,
            "#[derive(Clone, Debug, PartialEq)] pub enum {} {{",
            enumeration.name
        )
        .expect("String writes do not fail");
        for variant in &enumeration.variants {
            if let Some(payload) = &variant.payload {
                writeln!(output, "    {}({}),", variant.name, rust_type(payload))
                    .expect("String writes do not fail");
            } else {
                writeln!(output, "    {},", variant.name).expect("String writes do not fail");
            }
        }
        output.push_str("}\n");
        writeln!(
            output,
            "impl {} {{ pub const fn nexa_tag(&self) -> u32 {{ match self {{",
            enumeration.name
        )
        .expect("String writes do not fail");
        for (tag, variant) in enumeration.variants.iter().enumerate() {
            let pattern = if variant.payload.is_some() {
                format!("Self::{}(_)", variant.name)
            } else {
                format!("Self::{}", variant.name)
            };
            writeln!(output, "{pattern} => {tag},").expect("String writes do not fail");
        }
        output.push_str("} } }\n");
        writeln!(
            output,
            "#[derive(Clone, Copy, Debug)] pub enum {}Ref<'a> {{",
            enumeration.name
        )
        .expect("String writes do not fail");
        for variant in &enumeration.variants {
            if let Some(payload) = &variant.payload {
                writeln!(
                    output,
                    "    {}({}),",
                    variant.name,
                    input_rust_type(idl, payload, "'a")
                )
                .expect("String writes do not fail");
            } else {
                writeln!(output, "    {},", variant.name).expect("String writes do not fail");
            }
        }
        output.push_str("    #[doc(hidden)] __Lifetime(std::marker::PhantomData<&'a ()>),\n");
        output.push_str("}\n");
        writeln!(
            output,
            "impl<'a> {}Ref<'a> {{ fn from_runtime(value: \
             nexa_runtime::HostEnumRef<'a>) -> Result<Self, nexa_runtime::HostTrap> {{",
            enumeration.name
        )
        .expect("String writes do not fail");
        writeln!(
            output,
            "if value.type_id() != nexa_runtime::StableId({}) {{ return \
             Err(nexa_runtime::HostTrap::Type); }} match (value.variant(), value.tag()) {{",
            StableId::from_name(&enumeration.name).0
        )
        .expect("String writes do not fail");
        for (tag, variant) in enumeration.variants.iter().enumerate() {
            let variant_id = StableId::from_parts(&[&enumeration.name, "::", &variant.name]).0;
            if let Some(payload) = &variant.payload {
                let decoded = decode_runtime_ref_value(
                    idl,
                    payload,
                    "value.payload().ok_or(nexa_runtime::HostTrap::Type)?",
                );
                writeln!(
                    output,
                    "(nexa_runtime::StableId({variant_id}), {tag}) => \
                     Ok(Self::{}({decoded})),",
                    variant.name
                )
                .expect("String writes do not fail");
            } else {
                writeln!(
                    output,
                    "(nexa_runtime::StableId({variant_id}), {tag}) if value.payload().is_none() \
                     => Ok(Self::{}),",
                    variant.name
                )
                .expect("String writes do not fail");
            }
        }
        output.push_str("_ => Err(nexa_runtime::HostTrap::Type), } } }\n");
    }
    for structure in &idl.structs {
        writeln!(
            output,
            "#[derive(Clone, Debug, PartialEq)] pub struct {} {{",
            structure.name
        )
        .expect("String writes do not fail");
        for field in &structure.fields {
            writeln!(output, "    pub {}: {},", field.name, rust_type(&field.ty))
                .expect("String writes do not fail");
        }
        output.push_str("}\n");
        writeln!(
            output,
            "#[derive(Clone, Copy, Debug)] pub struct {}Ref<'a>(\
             nexa_runtime::HostStructRef<'a>);",
            structure.name
        )
        .expect("String writes do not fail");
        writeln!(output, "impl<'a> {}Ref<'a> {{", structure.name)
            .expect("String writes do not fail");
        writeln!(
            output,
            "fn from_runtime(value: nexa_runtime::HostStructRef<'a>) -> Self {{ Self(value) }}"
        )
        .expect("String writes do not fail");
        for (index, field) in structure.fields.iter().enumerate() {
            let decoded =
                decode_runtime_ref_value(idl, &field.ty, &format!("self.0.field({index})?"));
            writeln!(
                output,
                "#[allow(clippy::needless_question_mark)] pub fn {}(self) -> Result<{}, \
                 nexa_runtime::HostTrap> {{ Ok({decoded}) }}",
                field.name,
                input_rust_type(idl, &field.ty, "'a")
            )
            .expect("String writes do not fail");
        }
        output.push_str("}\n");
    }
    for structure in &idl.structs {
        let mut nested_requirements = String::new();
        for field in &structure.fields {
            let item = requirements_value_expr(idl, &field.ty, &format!("&self.{}", field.name));
            write!(
                nested_requirements,
                "__nexa_requirements = __nexa_requirements.checked_add({item})?;"
            )
            .expect("String writes do not fail");
        }
        let requirements = format!(
            "{{ let mut __nexa_requirements = nexa_runtime::HostReturnRequirements {{ \
             object_slots: 1, struct_fields: {}, ..nexa_runtime::HostReturnRequirements::ZERO }}; \
             {nested_requirements} __nexa_requirements }}",
            structure.fields.len()
        );
        writeln!(
            output,
            "#[allow(clippy::identity_op)] impl nexa_runtime::EncodeHostReturn for {} {{ \
             fn requirements(&self) -> Result<nexa_runtime::HostReturnRequirements, \
             nexa_runtime::HostTrap> {{ Ok({requirements}) }} fn encode_into(self, transaction: \
             &mut nexa_runtime::HostReturnTransaction<'_>) -> Result<nexa_runtime::RuntimeValue, \
             nexa_runtime::HostTrap> {{",
            structure.name
        )
        .expect("String writes do not fail");
        output.push_str(
            "let mut __nexa_fields = [nexa_runtime::RuntimeValue::Unit; \
             nexa_runtime::MAX_HOST_RETURN_FIELDS];\n",
        );
        for (index, field) in structure.fields.iter().enumerate() {
            let encoded = encode_runtime_return_value(
                idl,
                &field.ty,
                &format!("self.{}", field.name),
                "transaction",
            );
            writeln!(output, "__nexa_fields[{index}] = {encoded};")
                .expect("String writes do not fail");
        }
        writeln!(
            output,
            "transaction.write_struct(nexa_runtime::StableId({}), \
             &__nexa_fields[..{}]) }} }}",
            StableId::from_name(&structure.name).0,
            structure.fields.len()
        )
        .expect("String writes do not fail");
    }
    for enumeration in &idl.enums {
        let requirements =
            requirements_value_expr(idl, &TypeRef::Named(enumeration.name.clone()), "self");
        let encoded = encode_runtime_return_value(
            idl,
            &TypeRef::Named(enumeration.name.clone()),
            "self",
            "transaction",
        );
        writeln!(
            output,
            "impl nexa_runtime::EncodeHostReturn for {} {{ \
             fn requirements(&self) -> Result<nexa_runtime::HostReturnRequirements, \
             nexa_runtime::HostTrap> {{ let requirements = {requirements}; Ok(requirements) }} \
             fn encode_into(self, transaction: \
             &mut nexa_runtime::HostReturnTransaction<'_>) -> Result<nexa_runtime::RuntimeValue, \
             nexa_runtime::HostTrap> {{ Ok({encoded}) }} }}",
            enumeration.name
        )
        .expect("String writes do not fail");
    }
    writeln!(output, "pub trait {} {{", idl.interface).expect("String writes do not fail");
    for function in &idl.functions {
        let lifetime = if function
            .parameters
            .iter()
            .any(|parameter| input_type_borrows(idl, &parameter.ty))
        {
            "<'a>"
        } else {
            ""
        };
        write!(
            output,
            "    #[allow(clippy::too_many_arguments)] fn {}{lifetime}(&mut self, context: &mut \
             nexa_runtime::ResourceContext<'_>",
            function.name,
        )
        .expect("String writes do not fail");
        for parameter in &function.parameters {
            write!(
                output,
                ", {}: {}",
                parameter.name,
                input_rust_type(idl, &parameter.ty, "'a")
            )
            .expect("String writes do not fail");
        }
        writeln!(
            output,
            ") -> Result<{}, HostError>;",
            rust_type(&function.result)
        )
        .expect("String writes do not fail");
    }
    output.push_str("}\n");
    writeln!(output, "pub struct GeneratedHostStub;").expect("String writes do not fail");
    writeln!(output, "impl {} for GeneratedHostStub {{", idl.interface)
        .expect("String writes do not fail");
    for function in &idl.functions {
        let lifetime = if function
            .parameters
            .iter()
            .any(|parameter| input_type_borrows(idl, &parameter.ty))
        {
            "<'a>"
        } else {
            ""
        };
        write!(
            output,
            "#[allow(clippy::too_many_arguments)] fn {}{lifetime}(&mut self, _context: &mut \
             nexa_runtime::ResourceContext<'_>",
            function.name,
        )
        .expect("String writes do not fail");
        for parameter in &function.parameters {
            write!(
                output,
                ", _{}: {}",
                parameter.name,
                input_rust_type(idl, &parameter.ty, "'a")
            )
            .expect("String writes do not fail");
        }
        writeln!(
            output,
            ") -> Result<{}, HostError> {{ Err(HostError({:?}.into())) }}",
            rust_type(&function.result),
            format!("unimplemented host function {}", function.name)
        )
        .expect("String writes do not fail");
    }
    output.push_str("}\n");
    for (index, function) in idl.functions.iter().enumerate() {
        writeln!(
            output,
            "pub const THUNK_{}: u32 = {index};",
            function.name.to_ascii_uppercase()
        )
        .expect("String writes do not fail");
        writeln!(
            output,
            "pub const FUNCTION_ID_{}: nexa_runtime::StableId = nexa_runtime::StableId({});",
            function.name.to_ascii_uppercase(),
            StableId::from_parts(&[&idl.interface, "::", &function.name]).0
        )
        .expect("String writes do not fail");
        if !function.synchronous {
            let TypeRef::HostRequest(Some(result)) = &function.result else {
                continue;
            };
            let TypeRef::Result(success, error) = result.as_ref() else {
                continue;
            };
            let ticket = format!("{}CompletionTicket", pascal_case(&function.name));
            let success_payload = encode_completion_payload(idl, success, "value");
            let error_code = encode_completion_error(idl, error, "error");
            writeln!(
                output,
                "pub struct {ticket}(pub nexa_runtime::HostCompletionTicket);\n\
                 impl {ticket} {{\n\
                 pub fn complete(&mut self, result: Result<{success}, {error}>) \
                 -> Result<(), nexa_runtime::HostRequestError> {{\n\
                 match result {{ Ok(value) => self.0.complete({success_payload}), \
                 Err(error) => self.0.fail(nexa_runtime::HostErrorPayload {{ code: {error_code} }}) }}\
                 \n}}\n}}",
                success = rust_type(success),
                error = rust_type(error),
            )
            .expect("String writes do not fail");
        }
    }
    writeln!(
        output,
        "pub struct GeneratedHostRegistry<H> {{ pub host: H }}\n\
         impl<H> GeneratedHostRegistry<H> {{ pub const fn new(host: H) -> Self {{ Self {{ host }} }} }}"
    )
    .expect("String writes do not fail");
    writeln!(
        output,
        "impl<H: {}> nexa_runtime::HostRegistry for GeneratedHostRegistry<H> {{",
        idl.interface
    )
    .expect("String writes do not fail");
    writeln!(
        output,
        "fn interface_hash(&self) -> Option<nexa_runtime::StableId> {{ \
         Some(nexa_runtime::StableId({})) }}",
        exact_hash(idl).0
    )
    .expect("String writes do not fail");
    output.push_str(
        "fn call_runtime(&mut self, id: u32, context: &mut \
         nexa_runtime::ResourceContext<'_>, args: nexa_runtime::RuntimeHostArgs<'_>) -> \
         Result<nexa_runtime::HostCallOutcome, nexa_runtime::HostTrap> {\n",
    );
    emit_host_dispatch(&mut output, idl);
    output.push_str("}\n}\n");
    writeln!(
        output,
        "pub fn registry<H: {} + 'static>(host: H) -> Box<dyn nexa_runtime::HostRegistry> {{ \
         Box::new(GeneratedHostRegistry::new(host)) }}",
        idl.interface
    )
    .expect("String writes do not fail");
    for (index, export) in idl.exports.iter().enumerate() {
        let args = if export.parameters.is_empty() {
            format!(
                "#[derive(Clone, Debug, PartialEq)] pub struct {}Args;",
                export.name
            )
        } else {
            let fields = export
                .parameters
                .iter()
                .map(|field| format!("pub {}: {}", field.name, rust_type(&field.ty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "#[derive(Clone, Debug, PartialEq)] pub struct {}Args {{ {fields} }}",
                export.name
            )
        };
        let output_type = export.result.as_ref().map_or("()".to_owned(), rust_type);
        let export_id = StableId::from_parts(&[&idl.interface, "::export::", &export.name]).0;
        let mut requirements = String::from(
            "let mut __nexa_requirements = \
             nexa_runtime::ScriptArgumentRequirements::ZERO;",
        );
        let mut encoded = String::new();
        let mut parameter_types = String::new();
        for parameter in &export.parameters {
            let requirement =
                requirements_value_expr(idl, &parameter.ty, &format!("&args.{}", parameter.name));
            write!(
                requirements,
                "__nexa_requirements = __nexa_requirements.checked_add({requirement})?;"
            )
            .expect("String writes do not fail");
            let value = encode_runtime_return_value(
                idl,
                &parameter.ty,
                &format!("args.{}.clone()", parameter.name),
                "writer",
            );
            write!(encoded, "{value},").expect("String writes do not fail");
            let value_type = runtime_value_type_expr(idl, &parameter.ty);
            write!(parameter_types, "{value_type},").expect("String writes do not fail");
        }
        let output_signature = export.result.as_ref().map_or_else(
            || "None".to_owned(),
            |result| format!("Some({})", runtime_value_type_expr(idl, result)),
        );
        let decoded_output = export.result.as_ref().map_or_else(
            || {
                "match value { nexa_runtime::RuntimeValue::Unit => (), _ => return \
                 Err(nexa_runtime::ScriptCallError::OutputDecoding) }"
                    .to_owned()
            },
            |result| decode_script_output_value(idl, result, "reader.value(value)"),
        );
        writeln!(
            output,
            "{args}\n\
             pub type {name}Output = {output_type};\n\
             pub enum {name} {{}}\n\
             impl nexa_runtime::ScriptExport for {name} {{\n\
             type Args = {name}Args; type Output = {name}Output;\n\
             const STABLE_ID: nexa_runtime::StableId = nexa_runtime::StableId({export_id});\n\
             const NAME: &'static str = \"{name}\";\n\
             fn signature() -> nexa_runtime::Signature {{ nexa_runtime::Signature {{ \
             parameters: vec![{parameter_types}], result: {output_signature} }} }}\n\
             fn argument_requirements(args: &Self::Args) -> \
             Result<nexa_runtime::ScriptArgumentRequirements, nexa_runtime::ScriptCallError> {{ \
             let _ = args; {requirements} Ok(__nexa_requirements) }}\n\
             #[allow(clippy::clone_on_copy)] \
             fn encode_args(writer: &mut nexa_runtime::ScriptCallWriter<'_>, args: &Self::Args) \
             -> Result<Vec<nexa_runtime::RuntimeValue>, nexa_runtime::ScriptCallError> {{ \
             let _ = writer; let _ = args; \
             let __nexa_values = vec![{encoded}]; \
             Ok(__nexa_values) }}\n\
             fn decode_output(reader: &nexa_runtime::ScriptOutputReader<'_>, \
             value: nexa_runtime::RuntimeValue) -> Result<Self::Output, \
             nexa_runtime::ScriptCallError> {{ let _ = reader; Ok({decoded_output}) }} }}\n\
             impl {name} {{ pub const EXPORT_NAME: &'static str = \"{name}\"; \
             pub const EXPORT_ID: nexa_runtime::StableId = nexa_runtime::StableId({export_id}); \
             pub const LEGACY_FUNCTION_INDEX: u32 = {index}; }}",
            name = export.name,
        )
        .expect("String writes do not fail");
    }
    output
}

fn emit_host_dispatch(output: &mut String, idl: &Idl) {
    output.push_str("match id {\n");
    for (index, function) in idl.functions.iter().enumerate() {
        if function.parameters.is_empty() {
            writeln!(
                output,
                "{index} => {{ if !args.is_empty() {{ return \
                 Err(nexa_runtime::HostTrap::Arity); }}"
            )
            .expect("String writes do not fail");
        } else {
            writeln!(
                output,
                "{index} => {{ if args.len() != {} {{ return \
                 Err(nexa_runtime::HostTrap::Arity); }}",
                function.parameters.len()
            )
            .expect("String writes do not fail");
        }
        for (argument, parameter) in function.parameters.iter().enumerate() {
            let (prelude, decoded) = decode_runtime_host_value(idl, &parameter.ty, argument);
            output.push_str(&prelude);
            writeln!(output, "let {} = {decoded};", parameter.name)
                .expect("String writes do not fail");
        }
        write!(
            output,
            "let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| \
             self.host.{}(context",
            function.name
        )
        .expect("String writes do not fail");
        for parameter in &function.parameters {
            write!(output, ", {}", parameter.name).expect("String writes do not fail");
        }
        output.push_str(
            "))).map_err(|_| nexa_runtime::HostTrap::Panicked)?\
             .map_err(|error| nexa_runtime::HostTrap::Host(\
             nexa_runtime::RuntimeMessage::inline(&error.0)))?;\n",
        );
        if matches!(function.result, TypeRef::HostRequest(_)) {
            output.push_str("Ok(nexa_runtime::HostCallOutcome::Pending(result)) }\n");
        } else if return_requires_writer(&function.result) {
            let requirements = requirements_value_expr(idl, &function.result, "&result");
            let encoded =
                encode_runtime_return_value(idl, &function.result, "result", "__nexa_transaction");
            writeln!(
                output,
                "let __nexa_requirements = {requirements}; let mut __nexa_return = \
                 args.return_transaction(__nexa_requirements)?; let __nexa_value = {{ let \
                 __nexa_transaction = &mut __nexa_return; {encoded} }}; \
                 let __nexa_value = __nexa_return.commit(__nexa_value)?; \
                 Ok(nexa_runtime::HostCallOutcome::RuntimeImmediate(__nexa_value)) }}"
            )
            .expect("String writes do not fail");
        } else {
            let encoded =
                encode_runtime_return_value(idl, &function.result, "result", "__nexa_unused");
            writeln!(
                output,
                "Ok(nexa_runtime::HostCallOutcome::RuntimeImmediate({encoded})) }}"
            )
            .expect("String writes do not fail");
        }
    }
    output.push_str("_ => Err(nexa_runtime::HostTrap::UnknownFunction(id)),\n}\n");
}

fn decode_runtime_host_value(idl: &Idl, ty: &TypeRef, index: usize) -> (String, String) {
    let direct = match ty {
        TypeRef::I32 => Some(format!("args.i32({index})?")),
        TypeRef::I64 => Some(format!("args.i64({index})?")),
        TypeRef::F32 => Some(format!("args.f32({index})?")),
        TypeRef::F64 => Some(format!("args.f64({index})?")),
        TypeRef::Bool => Some(format!("args.bool({index})?")),
        TypeRef::Rune => Some(format!("args.rune({index})?")),
        TypeRef::String => Some(format!("args.str_ref({index})?.as_str()")),
        TypeRef::HostRequest(_) => Some(format!("args.request({index})?")),
        TypeRef::ResourceToken(Some(inner)) => Some(format!(
            "{}::from_raw(args.token({index})?)",
            typed_handle_name(inner, "Token")
        )),
        TypeRef::Snapshot(Some(inner)) => Some(format!(
            "{}::try_from(args.snapshot({index})?).map_err(|_| \
             nexa_runtime::HostTrap::Type)?",
            typed_handle_name(inner, "Snapshot")
        )),
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => {
            Some(format!("{name}(args.opaque({index})?)"))
        }
        TypeRef::ResourceToken(None) | TypeRef::Snapshot(None) => {
            unreachable!("validated host handles are typed")
        }
        TypeRef::Array(_)
        | TypeRef::Buffer(_)
        | TypeRef::Option(_)
        | TypeRef::Result(_, _)
        | TypeRef::Named(_) => None,
    };
    direct.map_or_else(
        || {
            let source = format!("args.value_ref({index})?");
            (String::new(), decode_runtime_ref_value(idl, ty, &source))
        },
        |decoded| (String::new(), decoded),
    )
}

#[allow(clippy::too_many_lines)]
fn decode_runtime_ref_value(idl: &Idl, ty: &TypeRef, source: &str) -> String {
    match ty {
        TypeRef::I32 => format!("{source}.i32()?"),
        TypeRef::I64 => format!("{source}.i64()?"),
        TypeRef::F32 => format!("{source}.f32()?"),
        TypeRef::F64 => format!("{source}.f64()?"),
        TypeRef::Bool => format!("{source}.bool()?"),
        TypeRef::Rune => format!("{source}.rune()?"),
        TypeRef::String => format!("{source}.str_ref()?.as_str()"),
        TypeRef::HostRequest(_) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::HostRequest(value) \
             => value, _ => return Err(nexa_runtime::HostTrap::Type) }}"
        ),
        TypeRef::ResourceToken(Some(inner)) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::ResourceToken(value) \
             => {}::from_raw(value), _ => return Err(nexa_runtime::HostTrap::Type) }}",
            typed_handle_name(inner, "Token")
        ),
        TypeRef::Snapshot(Some(inner)) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::Snapshot(value) => \
             {}::try_from(value).map_err(|_| nexa_runtime::HostTrap::Type)?, _ => return \
             Err(nexa_runtime::HostTrap::Type) }}",
            typed_handle_name(inner, "Snapshot")
        ),
        TypeRef::ResourceToken(None) | TypeRef::Snapshot(None) => {
            unreachable!("validated handles are typed")
        }
        TypeRef::Array(inner) => format!(
            "{source}.array_ref(nexa_runtime::StableId({}))?",
            nexa_bytecode::array_type(value_type(idl, inner)).0
        ),
        TypeRef::Buffer(inner) => format!(
            "{source}.buffer_ref(nexa_runtime::StableId({}))?",
            nexa_bytecode::buffer_type(value_type(idl, inner)).0
        ),
        TypeRef::Option(inner) => {
            let metadata = nexa_bytecode::option_type(value_type(idl, inner));
            let none = &metadata.variants[0];
            let some = &metadata.variants[1];
            let payload = decode_runtime_ref_value(
                idl,
                inner,
                "__nexa_enum.payload().ok_or(nexa_runtime::HostTrap::Type)?",
            );
            format!(
                "{{ let __nexa_enum = {source}.enum_ref(nexa_runtime::StableId({type_id}))?; \
                 match (__nexa_enum.variant(), __nexa_enum.tag()) {{ \
                 (nexa_runtime::StableId({none_id}), {none_tag}) if \
                 __nexa_enum.payload().is_none() => None, \
                 (nexa_runtime::StableId({some_id}), {some_tag}) => Some({payload}), \
                 _ => return Err(nexa_runtime::HostTrap::Type), }} }}",
                type_id = metadata.type_id.0,
                none_id = none.stable_id.0,
                none_tag = none.tag,
                some_id = some.stable_id.0,
                some_tag = some.tag,
            )
        }
        TypeRef::Result(success, error) => {
            let metadata =
                nexa_bytecode::result_type(value_type(idl, success), value_type(idl, error));
            let ok = &metadata.variants[0];
            let err = &metadata.variants[1];
            let success = decode_runtime_ref_value(
                idl,
                success,
                "__nexa_enum.payload().ok_or(nexa_runtime::HostTrap::Type)?",
            );
            let error = decode_runtime_ref_value(
                idl,
                error,
                "__nexa_enum.payload().ok_or(nexa_runtime::HostTrap::Type)?",
            );
            format!(
                "{{ let __nexa_enum = {source}.enum_ref(nexa_runtime::StableId({type_id}))?; \
                 match (__nexa_enum.variant(), __nexa_enum.tag()) {{ \
                 (nexa_runtime::StableId({ok_id}), {ok_tag}) => Ok({success}), \
                 (nexa_runtime::StableId({err_id}), {err_tag}) => Err({error}), \
                 _ => return Err(nexa_runtime::HostTrap::Type), }} }}",
                type_id = metadata.type_id.0,
                ok_id = ok.stable_id.0,
                ok_tag = ok.tag,
                err_id = err.stable_id.0,
                err_tag = err.tag,
            )
        }
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::Opaque {{ value, \
             type_id }} if type_id == nexa_runtime::StableId({}) => {name}(value), \
             nexa_runtime::RuntimeValue::Ref(reference) | \
             nexa_runtime::RuntimeValue::NamedRef {{ reference, .. }} => \
             {name}(u64::from(reference.generation) << 32 | u64::from(reference.index)), \
             _ => return Err(nexa_runtime::HostTrap::Type) }}",
            StableId::from_name(name).0
        ),
        TypeRef::Named(name) if idl.enums.iter().any(|item| item.name == *name) => format!(
            "{name}Ref::from_runtime({source}.enum_ref(nexa_runtime::StableId({}))?)?",
            StableId::from_name(name).0
        ),
        TypeRef::Named(name) => format!(
            "{name}Ref::from_runtime({source}.struct_ref(nexa_runtime::StableId({}))?)",
            StableId::from_name(name).0
        ),
    }
}

fn return_requires_writer(ty: &TypeRef) -> bool {
    matches!(
        ty,
        TypeRef::String
            | TypeRef::Array(_)
            | TypeRef::Buffer(_)
            | TypeRef::Option(_)
            | TypeRef::Result(_, _)
            | TypeRef::Named(_)
    )
}

#[allow(clippy::too_many_lines)]
fn requirements_value_expr(idl: &Idl, ty: &TypeRef, source: &str) -> String {
    match ty {
        TypeRef::I32
        | TypeRef::I64
        | TypeRef::F32
        | TypeRef::F64
        | TypeRef::Bool
        | TypeRef::Rune
        | TypeRef::HostRequest(_)
        | TypeRef::ResourceToken(_)
        | TypeRef::Snapshot(_) => "nexa_runtime::HostReturnRequirements::ZERO".into(),
        TypeRef::String => format!(
            "nexa_runtime::HostReturnRequirements {{ object_slots: 1, string_bytes: \
             ({source}).len(), ..nexa_runtime::HostReturnRequirements::ZERO }}"
        ),
        TypeRef::Array(inner) => {
            let inner = requirements_value_expr(idl, inner, "value");
            format!(
                "{{ let mut __nexa_requirements = nexa_runtime::HostReturnRequirements {{ \
                 object_slots: 1, collection_elements: ({source}).len(), \
                 ..nexa_runtime::HostReturnRequirements::ZERO }}; for value in ({source}).iter() {{ \
                 let _ = value; \
                 __nexa_requirements = __nexa_requirements.checked_add({inner})?; }} \
                 __nexa_requirements }}"
            )
        }
        TypeRef::Buffer(inner) => {
            let inner = requirements_value_expr(idl, inner, "value");
            format!(
                "{{ let mut __nexa_requirements = nexa_runtime::HostReturnRequirements {{ \
                 object_slots: 1, collection_elements: ({source}).len(), \
                 ..nexa_runtime::HostReturnRequirements::ZERO }}; for value in \
                 ({source}).as_slice().iter() {{ let _ = value; __nexa_requirements = \
                 __nexa_requirements.checked_add({inner})?; }} __nexa_requirements }}"
            )
        }
        TypeRef::Option(inner) => {
            let inner = requirements_value_expr(idl, inner, "value");
            format!(
                "nexa_runtime::HostReturnRequirements {{ object_slots: 1, \
                 ..nexa_runtime::HostReturnRequirements::ZERO }}.checked_add(match {source} {{ \
                 Some(value) => {inner}, None => nexa_runtime::HostReturnRequirements::ZERO }})?"
            )
        }
        TypeRef::Result(success, error) => {
            let success = requirements_value_expr(idl, success, "value");
            let error = requirements_value_expr(idl, error, "error");
            format!(
                "nexa_runtime::HostReturnRequirements {{ object_slots: 1, \
                 ..nexa_runtime::HostReturnRequirements::ZERO }}.checked_add(match {source} {{ \
                 Ok(value) => {success}, Err(error) => {error} }})?"
            )
        }
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => {
            "nexa_runtime::HostReturnRequirements::ZERO".into()
        }
        TypeRef::Named(name) if idl.enums.iter().any(|item| item.name == *name) => {
            let enumeration = idl
                .enums
                .iter()
                .find(|item| item.name == *name)
                .expect("validated enum exists");
            let mut expression = format!(
                "nexa_runtime::HostReturnRequirements {{ object_slots: 1, \
                 ..nexa_runtime::HostReturnRequirements::ZERO }}.checked_add(match {source} {{"
            );
            for variant in &enumeration.variants {
                if let Some(payload) = &variant.payload {
                    let nested = requirements_value_expr(idl, payload, "value");
                    write!(
                        expression,
                        "{name}::{}(value) => {{ let _ = value; {nested} }},",
                        variant.name
                    )
                    .expect("String writes do not fail");
                } else {
                    write!(
                        expression,
                        "{name}::{} => nexa_runtime::HostReturnRequirements::ZERO,",
                        variant.name
                    )
                    .expect("String writes do not fail");
                }
            }
            expression.push_str("})?");
            expression
        }
        TypeRef::Named(_) => format!("nexa_runtime::EncodeHostReturn::requirements({source})?"),
    }
}

fn runtime_value_type_expr(idl: &Idl, ty: &TypeRef) -> String {
    match value_type(idl, ty) {
        ValueType::I32 => "nexa_runtime::ValueType::I32".into(),
        ValueType::I64 => "nexa_runtime::ValueType::I64".into(),
        ValueType::F32 => "nexa_runtime::ValueType::F32".into(),
        ValueType::F64 => "nexa_runtime::ValueType::F64".into(),
        ValueType::Bool => "nexa_runtime::ValueType::Bool".into(),
        ValueType::Rune => "nexa_runtime::ValueType::Rune".into(),
        ValueType::String => "nexa_runtime::ValueType::String".into(),
        ValueType::Ref => "nexa_runtime::ValueType::Ref".into(),
        ValueType::Named(type_id) => {
            format!(
                "nexa_runtime::ValueType::Named(nexa_runtime::StableId({}))",
                type_id.0
            )
        }
    }
}

#[allow(clippy::too_many_lines)]
fn encode_runtime_return_value(idl: &Idl, ty: &TypeRef, source: &str, writer: &str) -> String {
    match ty {
        TypeRef::I32 => format!("nexa_runtime::RuntimeValue::I32({source})"),
        TypeRef::I64 => format!("nexa_runtime::RuntimeValue::I64({source})"),
        TypeRef::F32 => format!("nexa_runtime::RuntimeValue::F32({source}.to_bits())"),
        TypeRef::F64 => format!("nexa_runtime::RuntimeValue::F64({source}.to_bits())"),
        TypeRef::Bool => format!("nexa_runtime::RuntimeValue::Bool({source})"),
        TypeRef::Rune => format!("nexa_runtime::RuntimeValue::Rune({source} as u32)"),
        TypeRef::String => format!("{writer}.write_string({source})?"),
        TypeRef::HostRequest(_) => unreachable!("requests are pending host outcomes"),
        TypeRef::ResourceToken(_) => {
            format!("nexa_runtime::RuntimeValue::ResourceToken({source}.into_raw())")
        }
        TypeRef::Snapshot(_) => {
            format!("nexa_runtime::RuntimeValue::Snapshot({source}.into_raw())")
        }
        TypeRef::Array(inner) => {
            let encoded = encode_runtime_return_value(idl, inner, "value", writer);
            let type_id = nexa_bytecode::array_type(value_type(idl, inner)).0;
            let element_type = runtime_value_type_expr(idl, inner);
            format!(
                "{{ let mut __nexa_array = {writer}.begin_array(nexa_runtime::StableId({type_id}), \
                 {element_type}, {source}.len())?; for value in {source} {{ let __nexa_encoded = \
                 {encoded}; {writer}.push_array_value(&mut __nexa_array, __nexa_encoded)?; }} \
                 {writer}.finish_array(__nexa_array)? }}"
            )
        }
        TypeRef::Buffer(inner) => {
            let encoded = encode_runtime_return_value(idl, inner, "value", writer);
            let type_id = nexa_bytecode::buffer_type(value_type(idl, inner)).0;
            let element_type = runtime_value_type_expr(idl, inner);
            format!(
                "{{ let mut __nexa_buffer = {writer}.begin_buffer(\
                 nexa_runtime::StableId({type_id}), {element_type}, {source}.len())?; for value in \
                 {source} {{ let __nexa_encoded = {encoded}; {writer}.push_buffer_value(\
                 &mut __nexa_buffer, __nexa_encoded)?; }} {writer}.finish_buffer(__nexa_buffer)? }}"
            )
        }
        TypeRef::Option(inner) => {
            let metadata = nexa_bytecode::option_type(value_type(idl, inner));
            let none = &metadata.variants[0];
            let some = &metadata.variants[1];
            let payload = encode_runtime_return_value(idl, inner, "value", writer);
            format!(
                "match {source} {{ Some(value) => {{ let __nexa_payload = {payload}; \
                 {writer}.write_enum(nexa_runtime::StableId({type_id}), \
                 nexa_runtime::StableId({some_id}), {some_tag}, Some(__nexa_payload))? }}, \
                 None => {writer}.write_enum(nexa_runtime::StableId({type_id}), \
                 nexa_runtime::StableId({none_id}), {none_tag}, None)?, }}",
                type_id = metadata.type_id.0,
                some_id = some.stable_id.0,
                some_tag = some.tag,
                none_id = none.stable_id.0,
                none_tag = none.tag,
            )
        }
        TypeRef::Result(success, error) => {
            let metadata =
                nexa_bytecode::result_type(value_type(idl, success), value_type(idl, error));
            let ok = &metadata.variants[0];
            let err = &metadata.variants[1];
            let success = encode_runtime_return_value(idl, success, "value", writer);
            let error = encode_runtime_return_value(idl, error, "error", writer);
            format!(
                "match {source} {{ Ok(value) => {{ let __nexa_payload = {success}; \
                 {writer}.write_enum(nexa_runtime::StableId({type_id}), \
                 nexa_runtime::StableId({ok_id}), {ok_tag}, Some(__nexa_payload))? }}, \
                 Err(error) => {{ let __nexa_payload = {error}; \
                 {writer}.write_enum(nexa_runtime::StableId({type_id}), \
                 nexa_runtime::StableId({err_id}), {err_tag}, Some(__nexa_payload))? }}, }}",
                type_id = metadata.type_id.0,
                ok_id = ok.stable_id.0,
                ok_tag = ok.tag,
                err_id = err.stable_id.0,
                err_tag = err.tag,
            )
        }
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => format!(
            "nexa_runtime::RuntimeValue::Opaque {{ value: {source}.0, type_id: \
             nexa_runtime::StableId({}) }}",
            StableId::from_name(name).0
        ),
        TypeRef::Named(name) if idl.enums.iter().any(|item| item.name == *name) => {
            let enumeration = idl
                .enums
                .iter()
                .find(|item| item.name == *name)
                .expect("validated enum exists");
            let type_id = StableId::from_name(name).0;
            let mut expression = format!("match {source} {{");
            for (tag, variant) in enumeration.variants.iter().enumerate() {
                let variant_id = StableId::from_parts(&[name, "::", &variant.name]).0;
                if let Some(payload) = &variant.payload {
                    let payload = encode_runtime_return_value(idl, payload, "value", writer);
                    write!(
                        expression,
                        "{name}::{}(value) => {{ let __nexa_payload = {payload}; \
                         {writer}.write_enum(nexa_runtime::StableId({type_id}), \
                         nexa_runtime::StableId({variant_id}), {tag}, \
                         Some(__nexa_payload))? }},",
                        variant.name
                    )
                    .expect("String writes do not fail");
                } else {
                    write!(
                        expression,
                        "{name}::{} => {writer}.write_enum(\
                         nexa_runtime::StableId({type_id}), \
                         nexa_runtime::StableId({variant_id}), {tag}, None)?,",
                        variant.name
                    )
                    .expect("String writes do not fail");
                }
            }
            expression.push('}');
            expression
        }
        TypeRef::Named(_) => {
            format!("nexa_runtime::EncodeHostReturn::encode_into({source}, {writer})?")
        }
    }
}

#[allow(clippy::too_many_lines)]
fn decode_script_output_value(idl: &Idl, ty: &TypeRef, source: &str) -> String {
    match ty {
        TypeRef::I32 => format!("{source}.i32()?"),
        TypeRef::I64 => format!("{source}.i64()?"),
        TypeRef::F32 => format!("{source}.f32()?"),
        TypeRef::F64 => format!("{source}.f64()?"),
        TypeRef::Bool => format!("{source}.bool()?"),
        TypeRef::Rune => format!("{source}.rune()?"),
        TypeRef::String => format!("{source}.str_ref()?.as_str().to_owned()"),
        TypeRef::HostRequest(_) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::HostRequest(value) => \
             value, _ => return Err(nexa_runtime::ScriptCallError::OutputDecoding) }}"
        ),
        TypeRef::ResourceToken(Some(inner)) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::ResourceToken(value) => \
             {}::from_raw(value), _ => return \
             Err(nexa_runtime::ScriptCallError::OutputDecoding) }}",
            typed_handle_name(inner, "Token")
        ),
        TypeRef::Snapshot(Some(inner)) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::Snapshot(value) => \
             {}::try_from(value).map_err(|_| nexa_runtime::ScriptCallError::OutputDecoding)?, \
             _ => return Err(nexa_runtime::ScriptCallError::OutputDecoding) }}",
            typed_handle_name(inner, "Snapshot")
        ),
        TypeRef::ResourceToken(None) | TypeRef::Snapshot(None) => {
            unreachable!("validated handles are typed")
        }
        TypeRef::Array(inner) | TypeRef::Buffer(inner) => {
            let type_id = match ty {
                TypeRef::Array(_) => nexa_bytecode::array_type(value_type(idl, inner)),
                TypeRef::Buffer(_) => nexa_bytecode::buffer_type(value_type(idl, inner)),
                _ => unreachable!(),
            };
            let accessor = if matches!(ty, TypeRef::Array(_)) {
                "array_ref"
            } else {
                "buffer_ref"
            };
            let decoded = decode_script_output_value(idl, inner, "__nexa_value");
            let collection = format!(
                "{{ let __nexa_collection = {source}.{accessor}(nexa_runtime::StableId({}))?; \
                 let mut __nexa_output = Vec::with_capacity(__nexa_collection.len()); \
                 for __nexa_value in __nexa_collection.iter() {{ \
                 __nexa_output.push({decoded}); }} __nexa_output }}",
                type_id.0
            );
            if matches!(ty, TypeRef::Buffer(_)) {
                format!("nexa_runtime::CopyBuffer::new({collection})")
            } else {
                collection
            }
        }
        TypeRef::Option(inner) => {
            let metadata = nexa_bytecode::option_type(value_type(idl, inner));
            let none = &metadata.variants[0];
            let some = &metadata.variants[1];
            let payload = decode_script_output_value(
                idl,
                inner,
                "__nexa_enum.payload().ok_or(nexa_runtime::ScriptCallError::OutputDecoding)?",
            );
            format!(
                "{{ let __nexa_enum = {source}.enum_ref(nexa_runtime::StableId({type_id}))?; \
                 match (__nexa_enum.variant(), __nexa_enum.tag()) {{ \
                 (nexa_runtime::StableId({none_id}), {none_tag}) if \
                 __nexa_enum.payload().is_none() => None, \
                 (nexa_runtime::StableId({some_id}), {some_tag}) => Some({payload}), \
                 _ => return Err(nexa_runtime::ScriptCallError::OutputDecoding), }} }}",
                type_id = metadata.type_id.0,
                none_id = none.stable_id.0,
                none_tag = none.tag,
                some_id = some.stable_id.0,
                some_tag = some.tag,
            )
        }
        TypeRef::Result(success, error) => {
            let metadata =
                nexa_bytecode::result_type(value_type(idl, success), value_type(idl, error));
            let ok = &metadata.variants[0];
            let err = &metadata.variants[1];
            let success = decode_script_output_value(
                idl,
                success,
                "__nexa_enum.payload().ok_or(nexa_runtime::ScriptCallError::OutputDecoding)?",
            );
            let error = decode_script_output_value(
                idl,
                error,
                "__nexa_enum.payload().ok_or(nexa_runtime::ScriptCallError::OutputDecoding)?",
            );
            format!(
                "{{ let __nexa_enum = {source}.enum_ref(nexa_runtime::StableId({type_id}))?; \
                 match (__nexa_enum.variant(), __nexa_enum.tag()) {{ \
                 (nexa_runtime::StableId({ok_id}), {ok_tag}) => Ok({success}), \
                 (nexa_runtime::StableId({err_id}), {err_tag}) => Err({error}), \
                 _ => return Err(nexa_runtime::ScriptCallError::OutputDecoding), }} }}",
                type_id = metadata.type_id.0,
                ok_id = ok.stable_id.0,
                ok_tag = ok.tag,
                err_id = err.stable_id.0,
                err_tag = err.tag,
            )
        }
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => format!(
            "match {source}.runtime_value() {{ nexa_runtime::RuntimeValue::Opaque {{ value, \
             type_id }} if type_id == nexa_runtime::StableId({}) => {name}(value), \
             _ => return Err(nexa_runtime::ScriptCallError::OutputDecoding) }}",
            StableId::from_name(name).0
        ),
        TypeRef::Named(name) if idl.enums.iter().any(|item| item.name == *name) => {
            let enumeration = idl
                .enums
                .iter()
                .find(|item| item.name == *name)
                .expect("validated enum exists");
            let mut expression = format!(
                "{{ let __nexa_enum = {source}.enum_ref(nexa_runtime::StableId({}))?; \
                 match (__nexa_enum.variant(), __nexa_enum.tag()) {{",
                StableId::from_name(name).0
            );
            for (tag, variant) in enumeration.variants.iter().enumerate() {
                let variant_id = StableId::from_parts(&[name, "::", &variant.name]).0;
                if let Some(payload) = &variant.payload {
                    let payload = decode_script_output_value(
                        idl,
                        payload,
                        "__nexa_enum.payload().ok_or(\
                         nexa_runtime::ScriptCallError::OutputDecoding)?",
                    );
                    write!(
                        expression,
                        "(nexa_runtime::StableId({variant_id}), {tag}) => \
                         {name}::{}({payload}),",
                        variant.name
                    )
                    .expect("String writes do not fail");
                } else {
                    write!(
                        expression,
                        "(nexa_runtime::StableId({variant_id}), {tag}) if \
                         __nexa_enum.payload().is_none() => {name}::{},",
                        variant.name
                    )
                    .expect("String writes do not fail");
                }
            }
            expression
                .push_str("_ => return Err(nexa_runtime::ScriptCallError::OutputDecoding), } }");
            expression
        }
        TypeRef::Named(name) => {
            let structure = idl
                .structs
                .iter()
                .find(|item| item.name == *name)
                .expect("validated struct exists");
            let mut expression = format!(
                "{{ let __nexa_struct = {source}.struct_ref(nexa_runtime::StableId({}))?; \
                 {name} {{",
                StableId::from_name(name).0
            );
            for (index, field) in structure.fields.iter().enumerate() {
                let decoded = decode_script_output_value(
                    idl,
                    &field.ty,
                    &format!("__nexa_struct.field({index})?"),
                );
                write!(expression, "{}: {decoded},", field.name)
                    .expect("String writes do not fail");
            }
            expression.push_str("} }");
            expression
        }
    }
}

fn collect_typed_handles(idl: &Idl) -> (BTreeSet<String>, BTreeSet<String>) {
    fn collect(ty: &TypeRef, tokens: &mut BTreeSet<String>, snapshots: &mut BTreeSet<String>) {
        match ty {
            TypeRef::ResourceToken(Some(inner)) => {
                let TypeRef::Named(name) = inner.as_ref() else {
                    unreachable!("validated resource domains are nominal")
                };
                tokens.insert(name.clone());
            }
            TypeRef::Snapshot(Some(inner)) => {
                let TypeRef::Named(name) = inner.as_ref() else {
                    unreachable!("validated snapshot contents are nominal")
                };
                snapshots.insert(name.clone());
            }
            TypeRef::HostRequest(Some(inner))
            | TypeRef::Array(inner)
            | TypeRef::Buffer(inner)
            | TypeRef::Option(inner) => collect(inner, tokens, snapshots),
            TypeRef::Result(success, error) => {
                collect(success, tokens, snapshots);
                collect(error, tokens, snapshots);
            }
            TypeRef::I32
            | TypeRef::I64
            | TypeRef::F32
            | TypeRef::F64
            | TypeRef::Bool
            | TypeRef::Rune
            | TypeRef::String
            | TypeRef::HostRequest(None)
            | TypeRef::ResourceToken(None)
            | TypeRef::Snapshot(None)
            | TypeRef::Named(_) => {}
        }
    }

    let mut tokens = BTreeSet::new();
    let mut snapshots = BTreeSet::new();
    for structure in &idl.structs {
        for field in &structure.fields {
            collect(&field.ty, &mut tokens, &mut snapshots);
        }
    }
    for enumeration in &idl.enums {
        for variant in &enumeration.variants {
            if let Some(payload) = &variant.payload {
                collect(payload, &mut tokens, &mut snapshots);
            }
        }
    }
    for function in &idl.functions {
        for parameter in &function.parameters {
            collect(&parameter.ty, &mut tokens, &mut snapshots);
        }
        collect(&function.result, &mut tokens, &mut snapshots);
    }
    for export in &idl.exports {
        for parameter in &export.parameters {
            collect(&parameter.ty, &mut tokens, &mut snapshots);
        }
        if let Some(result) = &export.result {
            collect(result, &mut tokens, &mut snapshots);
        }
    }
    (tokens, snapshots)
}

fn pascal_case(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + characters.as_str()
            })
        })
        .collect()
}

fn encode_completion_payload(idl: &Idl, ty: &TypeRef, source: &str) -> String {
    match ty {
        TypeRef::I32 => format!("nexa_runtime::HostPayload::I32({source})"),
        TypeRef::I64 => format!("nexa_runtime::HostPayload::I64({source})"),
        TypeRef::F32 => format!("nexa_runtime::HostPayload::F32({source}.to_bits())"),
        TypeRef::F64 => format!("nexa_runtime::HostPayload::F64({source}.to_bits())"),
        TypeRef::Bool => format!("nexa_runtime::HostPayload::Bool({source})"),
        TypeRef::Rune => format!("nexa_runtime::HostPayload::Rune({source} as u32)"),
        TypeRef::String => format!("nexa_runtime::HostPayload::String({source})"),
        TypeRef::ResourceToken(_) => {
            format!("nexa_runtime::HostPayload::Token({source}.into_raw())")
        }
        TypeRef::Snapshot(_) => {
            format!("nexa_runtime::HostPayload::Snapshot({source}.into_raw())")
        }
        TypeRef::Array(inner) => format!(
            "nexa_runtime::HostPayload::Array(nexa_runtime::CopyBuffer::new({source}.into_iter()\
             .map(|value| {}).collect()))",
            encode_completion_payload(idl, inner, "value")
        ),
        TypeRef::Buffer(inner) => format!(
            "nexa_runtime::HostPayload::Buffer(nexa_runtime::CopyBuffer::new({source}.into_vec()\
             .into_iter().map(|value| {}).collect()))",
            encode_completion_payload(idl, inner, "value")
        ),
        TypeRef::Option(inner) => encode_option(
            idl,
            inner,
            source,
            "nexa_runtime::HostPayload",
            encode_completion_payload,
        ),
        TypeRef::Result(success, error) => encode_result(
            idl,
            success,
            error,
            source,
            "nexa_runtime::HostPayload",
            encode_completion_payload,
        ),
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => {
            format!("nexa_runtime::HostPayload::Opaque({source}.0)")
        }
        TypeRef::Named(name) if idl.enums.iter().any(|item| item.name == *name) => {
            encode_enum_completion_payload(
                idl,
                idl.enums
                    .iter()
                    .find(|enumeration| enumeration.name == *name)
                    .expect("validated enum exists"),
                source,
            )
        }
        TypeRef::Named(name) if idl.structs.iter().any(|item| item.name == *name) => {
            let structure = idl
                .structs
                .iter()
                .find(|structure| structure.name == *name)
                .expect("validated struct exists");
            let fields = structure
                .fields
                .iter()
                .map(|field| {
                    encode_completion_payload(idl, &field.ty, &format!("{source}.{}", field.name))
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("nexa_runtime::HostPayload::structure([{fields}])")
        }
        _ => "nexa_runtime::HostPayload::Unit".into(),
    }
}

fn encode_enum_completion_payload(idl: &Idl, enumeration: &Enum, source: &str) -> String {
    let mut output = format!("match {source} {{");
    for (tag, variant) in enumeration.variants.iter().enumerate() {
        let variant_id = format!(
            "nexa_runtime::StableId::from_parts(&[\"{}\", \"::\", \"{}\"])",
            enumeration.name, variant.name
        );
        let type_id = format!(
            "nexa_runtime::StableId::from_name(\"{}\")",
            enumeration.name
        );
        if let Some(payload_type) = &variant.payload {
            let payload = encode_completion_payload(idl, payload_type, "value");
            write!(
                output,
                "{}::{}(value) => nexa_runtime::HostPayload::Enum {{ type_id: {type_id}, \
                 variant: {variant_id}, tag: {tag}, payload: Some(Box::new({payload})) }},",
                enumeration.name, variant.name
            )
            .expect("String writes do not fail");
        } else {
            write!(
                output,
                "{}::{} => nexa_runtime::HostPayload::Enum {{ type_id: {type_id}, \
                 variant: {variant_id}, tag: {tag}, payload: None }},",
                enumeration.name, variant.name
            )
            .expect("String writes do not fail");
        }
    }
    output.push('}');
    output
}

fn encode_completion_error(idl: &Idl, ty: &TypeRef, source: &str) -> String {
    match ty {
        TypeRef::I32 => format!("u32::from_ne_bytes({source}.to_ne_bytes())"),
        TypeRef::Named(name) if idl.enums.iter().any(|item| item.name == *name) => {
            format!("{source}.nexa_tag()")
        }
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => {
            format!("{source}.0 as u32")
        }
        _ => "u32::MAX".into(),
    }
}

fn encode_option(
    idl: &Idl,
    inner: &TypeRef,
    source: &str,
    value_path: &str,
    encode: fn(&Idl, &TypeRef, &str) -> String,
) -> String {
    let metadata = nexa_bytecode::option_type(value_type(idl, inner));
    let none = &metadata.variants[0];
    let some = &metadata.variants[1];
    let payload = encode(idl, inner, "value");
    format!(
        "match {source} {{ Some(value) => {value_path}::Enum {{ type_id: \
         nexa_runtime::StableId({type_id}), variant: nexa_runtime::StableId({some_id}), tag: \
         {some_tag}, payload: Some(Box::new({payload})) }}, None => {value_path}::Enum {{ type_id: \
         nexa_runtime::StableId({type_id}), variant: nexa_runtime::StableId({none_id}), tag: \
         {none_tag}, payload: None }} }}",
        type_id = metadata.type_id.0,
        some_id = some.stable_id.0,
        some_tag = some.tag,
        none_id = none.stable_id.0,
        none_tag = none.tag,
    )
}

fn encode_result(
    idl: &Idl,
    success: &TypeRef,
    error: &TypeRef,
    source: &str,
    value_path: &str,
    encode: fn(&Idl, &TypeRef, &str) -> String,
) -> String {
    let metadata = nexa_bytecode::result_type(value_type(idl, success), value_type(idl, error));
    let ok = &metadata.variants[0];
    let err = &metadata.variants[1];
    let success_payload = encode(idl, success, "value");
    let error_payload = encode(idl, error, "error");
    format!(
        "match {source} {{ Ok(value) => {value_path}::Enum {{ type_id: \
         nexa_runtime::StableId({type_id}), variant: nexa_runtime::StableId({ok_id}), tag: \
         {ok_tag}, payload: Some(Box::new({success_payload})) }}, Err(error) => \
         {value_path}::Enum {{ type_id: nexa_runtime::StableId({type_id}), variant: \
         nexa_runtime::StableId({err_id}), tag: {err_tag}, payload: \
         Some(Box::new({error_payload})) }} }}",
        type_id = metadata.type_id.0,
        ok_id = ok.stable_id.0,
        ok_tag = ok.tag,
        err_id = err.stable_id.0,
        err_tag = err.tag,
    )
}

#[allow(clippy::too_many_lines)]
fn snapshot_encode_statements(idl: &Idl, ty: &TypeRef, source: &str, bytes: &str) -> String {
    match ty {
        TypeRef::I32 | TypeRef::I64 => {
            format!("{bytes}.extend_from_slice(&(*{source}).to_le_bytes());")
        }
        TypeRef::F32 | TypeRef::F64 => {
            format!("{bytes}.extend_from_slice(&(*{source}).to_bits().to_le_bytes());")
        }
        TypeRef::Bool => format!("{bytes}.push(u8::from(*{source}));"),
        TypeRef::Rune => {
            format!("{bytes}.extend_from_slice(&u32::from(*{source}).to_le_bytes());")
        }
        TypeRef::String => format!(
            "let __nexa_len = u32::try_from(({source}).len()).map_err(|_| \
             HostError(\"snapshot string is too large\".into()))?; \
             {bytes}.extend_from_slice(&__nexa_len.to_le_bytes()); \
             {bytes}.extend_from_slice(({source}).as_bytes());"
        ),
        TypeRef::Array(inner) => {
            let item = snapshot_encode_statements(idl, inner, "__nexa_item", bytes);
            format!(
                "let __nexa_len = u32::try_from(({source}).len()).map_err(|_| \
                 HostError(\"snapshot array is too large\".into()))?; \
                 {bytes}.extend_from_slice(&__nexa_len.to_le_bytes()); \
                 for __nexa_item in ({source}).iter() {{ {item} }}"
            )
        }
        TypeRef::Buffer(inner) => {
            let item = snapshot_encode_statements(idl, inner, "__nexa_item", bytes);
            format!(
                "let __nexa_len = u32::try_from(({source}).len()).map_err(|_| \
                 HostError(\"snapshot buffer is too large\".into()))?; \
                 {bytes}.extend_from_slice(&__nexa_len.to_le_bytes()); \
                 for __nexa_item in ({source}).as_slice() {{ {item} }}"
            )
        }
        TypeRef::Option(inner) => {
            let some = snapshot_encode_statements(idl, inner, "__nexa_value", bytes);
            format!(
                "match {source} {{ Some(__nexa_value) => {{ {bytes}.push(1); {some} }}, \
                 None => {bytes}.push(0), }}"
            )
        }
        TypeRef::Result(success, error) => {
            let ok = snapshot_encode_statements(idl, success, "__nexa_value", bytes);
            let error = snapshot_encode_statements(idl, error, "__nexa_error", bytes);
            format!(
                "match {source} {{ Ok(__nexa_value) => {{ {bytes}.push(0); {ok} }}, \
                 Err(__nexa_error) => {{ {bytes}.push(1); {error} }}, }}"
            )
        }
        TypeRef::Named(name) => {
            if let Some(structure) = idl.structs.iter().find(|item| item.name == *name) {
                structure
                    .fields
                    .iter()
                    .map(|field| {
                        snapshot_encode_statements(
                            idl,
                            &field.ty,
                            &format!("&({source}).{}", field.name),
                            bytes,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else if let Some(enumeration) = idl.enums.iter().find(|item| item.name == *name) {
                let arms = enumeration
                    .variants
                    .iter()
                    .enumerate()
                    .map(|(tag, variant)| {
                        variant.payload.as_ref().map_or_else(
                            || {
                                format!(
                                    "{name}::{} => {bytes}.extend_from_slice(&{tag}_u32.to_le_bytes()),",
                                    variant.name
                                )
                            },
                            |payload| {
                                let encoded = snapshot_encode_statements(
                                    idl,
                                    payload,
                                    "__nexa_payload",
                                    bytes,
                                );
                                format!(
                                    "{name}::{}(__nexa_payload) => {{ \
                                     {bytes}.extend_from_slice(&{tag}_u32.to_le_bytes()); \
                                     {encoded} }},",
                                    variant.name
                                )
                            },
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("match {source} {{ {arms} }}")
            } else {
                format!("{bytes}.extend_from_slice(&({source}).0.to_le_bytes());")
            }
        }
        TypeRef::HostRequest(_) | TypeRef::ResourceToken(_) | TypeRef::Snapshot(_) => {
            "return Err(HostError(\"host handles cannot be embedded in snapshots\".into()));".into()
        }
    }
}

#[allow(clippy::too_many_lines)]
fn snapshot_decode_expression(idl: &Idl, ty: &TypeRef, payload: &str, cursor: &str) -> String {
    let fixed = |size: usize, conversion: &str| {
        format!(
            "{{ let __nexa_end = {cursor}.checked_add({size}).ok_or(\
             nexa_runtime::HostTrap::Type)?; let __nexa_slice = {payload}.get({cursor}..__nexa_end)\
             .ok_or(nexa_runtime::HostTrap::Type)?; {cursor} = __nexa_end; \
             {conversion} }}"
        )
    };
    match ty {
        TypeRef::I32 => fixed(
            4,
            "i32::from_le_bytes(__nexa_slice.try_into().map_err(|_| nexa_runtime::HostTrap::Type)?)",
        ),
        TypeRef::I64 => fixed(
            8,
            "i64::from_le_bytes(__nexa_slice.try_into().map_err(|_| nexa_runtime::HostTrap::Type)?)",
        ),
        TypeRef::F32 => fixed(
            4,
            "f32::from_bits(u32::from_le_bytes(__nexa_slice.try_into().map_err(|_| \
             nexa_runtime::HostTrap::Type)?))",
        ),
        TypeRef::F64 => fixed(
            8,
            "f64::from_bits(u64::from_le_bytes(__nexa_slice.try_into().map_err(|_| \
             nexa_runtime::HostTrap::Type)?))",
        ),
        TypeRef::Bool => format!(
            "{{ let __nexa_value = *{payload}.get({cursor}).ok_or(\
             nexa_runtime::HostTrap::Type)?; {cursor} += 1; match __nexa_value {{ \
             0 => false, 1 => true, _ => return Err(nexa_runtime::HostTrap::Type), }} }}"
        ),
        TypeRef::Rune => {
            let value = fixed(
                4,
                "u32::from_le_bytes(__nexa_slice.try_into().map_err(|_| \
                 nexa_runtime::HostTrap::Type)?)",
            );
            format!("char::from_u32({value}).ok_or(nexa_runtime::HostTrap::Type)?")
        }
        TypeRef::String => {
            let length = snapshot_decode_expression(idl, &TypeRef::I32, payload, cursor);
            format!(
                "{{ let __nexa_len = usize::try_from({length}).map_err(|_| \
                 nexa_runtime::HostTrap::Type)?; let __nexa_end = {cursor}.checked_add(__nexa_len)\
                 .ok_or(nexa_runtime::HostTrap::Type)?; let __nexa_value = std::str::from_utf8(\
                 {payload}.get({cursor}..__nexa_end).ok_or(nexa_runtime::HostTrap::Type)?)\
                 .map_err(|_| nexa_runtime::HostTrap::Type)?.to_owned(); {cursor} = __nexa_end; \
                 __nexa_value }}"
            )
        }
        TypeRef::Array(inner) => {
            let length = snapshot_decode_expression(idl, &TypeRef::I32, payload, cursor);
            let item = snapshot_decode_expression(idl, inner, payload, cursor);
            format!(
                "{{ let __nexa_len = usize::try_from({length}).map_err(|_| \
                 nexa_runtime::HostTrap::Type)?; let mut __nexa_values = \
                 Vec::with_capacity(__nexa_len); for _ in 0..__nexa_len {{ \
                 __nexa_values.push({item}); }} __nexa_values }}"
            )
        }
        TypeRef::Buffer(inner) => {
            let array =
                snapshot_decode_expression(idl, &TypeRef::Array(inner.clone()), payload, cursor);
            format!("nexa_runtime::CopyBuffer::new({array})")
        }
        TypeRef::Option(inner) => {
            let value = snapshot_decode_expression(idl, inner, payload, cursor);
            format!(
                "{{ let __nexa_tag = *{payload}.get({cursor}).ok_or(\
                 nexa_runtime::HostTrap::Type)?; {cursor} += 1; match __nexa_tag {{ \
                 0 => None, 1 => Some({value}), _ => return Err(nexa_runtime::HostTrap::Type), }} }}"
            )
        }
        TypeRef::Result(success, error) => {
            let success = snapshot_decode_expression(idl, success, payload, cursor);
            let error = snapshot_decode_expression(idl, error, payload, cursor);
            format!(
                "{{ let __nexa_tag = *{payload}.get({cursor}).ok_or(\
                 nexa_runtime::HostTrap::Type)?; {cursor} += 1; match __nexa_tag {{ \
                 0 => Ok({success}), 1 => Err({error}), \
                 _ => return Err(nexa_runtime::HostTrap::Type), }} }}"
            )
        }
        TypeRef::Named(name) => {
            if let Some(structure) = idl.structs.iter().find(|item| item.name == *name) {
                let fields = structure
                    .fields
                    .iter()
                    .map(|field| {
                        format!(
                            "{}: {},",
                            field.name,
                            snapshot_decode_expression(idl, &field.ty, payload, cursor)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{name} {{ {fields} }}")
            } else if let Some(enumeration) = idl.enums.iter().find(|item| item.name == *name) {
                let tag = snapshot_decode_expression(idl, &TypeRef::I32, payload, cursor);
                let arms = enumeration
                    .variants
                    .iter()
                    .enumerate()
                    .map(|(tag, variant)| {
                        variant.payload.as_ref().map_or_else(
                            || format!("{tag} => {name}::{},", variant.name),
                            |item| {
                                format!(
                                    "{tag} => {name}::{}({}),",
                                    variant.name,
                                    snapshot_decode_expression(idl, item, payload, cursor)
                                )
                            },
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "{{ let __nexa_tag = {tag}; match __nexa_tag {{ {arms} \
                     _ => return Err(nexa_runtime::HostTrap::Type), }} }}"
                )
            } else {
                let value = fixed(
                    8,
                    "u64::from_le_bytes(__nexa_slice.try_into().map_err(|_| \
                     nexa_runtime::HostTrap::Type)?)",
                );
                format!("{name}({value})")
            }
        }
        TypeRef::HostRequest(_) | TypeRef::ResourceToken(_) | TypeRef::Snapshot(_) => {
            "return Err(nexa_runtime::HostTrap::Type)".into()
        }
    }
}

fn type_name(ty: &TypeRef) -> String {
    match ty {
        TypeRef::I32 => "i32".into(),
        TypeRef::I64 => "i64".into(),
        TypeRef::F32 => "f32".into(),
        TypeRef::F64 => "f64".into(),
        TypeRef::Bool => "bool".into(),
        TypeRef::Rune => "rune".into(),
        TypeRef::String => "string".into(),
        TypeRef::HostRequest(inner) => parameterized_type_name("request", inner.as_deref()),
        TypeRef::ResourceToken(inner) => parameterized_type_name("token", inner.as_deref()),
        TypeRef::Snapshot(inner) => parameterized_type_name("snapshot", inner.as_deref()),
        TypeRef::Array(inner) => format!("array<{}>", type_name(inner)),
        TypeRef::Buffer(inner) => format!("buffer<{}>", type_name(inner)),
        TypeRef::Option(inner) => format!("Option<{}>", type_name(inner)),
        TypeRef::Result(success, error) => {
            format!("Result<{},{}>", type_name(success), type_name(error))
        }
        TypeRef::Named(name) => name.clone(),
    }
}

fn abi_type_descriptor(idl: &Idl, ty: &TypeRef) -> String {
    let lowered = match value_type(idl, ty) {
        ValueType::I32 => "i32".into(),
        ValueType::I64 => "i64".into(),
        ValueType::F32 => "f32".into(),
        ValueType::F64 => "f64".into(),
        ValueType::Bool => "bool".into(),
        ValueType::Rune => "rune".into(),
        ValueType::String => "string".into(),
        ValueType::Ref => "ref".into(),
        ValueType::Named(id) => format!("named#{:016x}", id.0),
    };
    format!("{}[{lowered}]", type_name(ty))
}

fn parameterized_type_name(name: &str, inner: Option<&TypeRef>) -> String {
    inner.map_or_else(
        || name.to_owned(),
        |inner| format!("{name}<{}>", type_name(inner)),
    )
}

fn typed_handle_name(inner: &TypeRef, suffix: &str) -> String {
    let TypeRef::Named(name) = inner else {
        unreachable!("typed host handles use nominal domains")
    };
    format!("{name}{suffix}")
}

fn value_type(idl: &Idl, ty: &TypeRef) -> ValueType {
    match ty {
        TypeRef::I32 => ValueType::I32,
        TypeRef::I64 => ValueType::I64,
        TypeRef::F32 => ValueType::F32,
        TypeRef::F64 => ValueType::F64,
        TypeRef::Bool => ValueType::Bool,
        TypeRef::Rune => ValueType::Rune,
        TypeRef::String => ValueType::String,
        TypeRef::HostRequest(_) => ValueType::Named(StableId::from_name("HostRequest")),
        TypeRef::ResourceToken(_) => ValueType::Named(StableId::from_name("ResourceToken")),
        TypeRef::Snapshot(Some(content)) => {
            let ValueType::Named(content_type) = value_type(idl, content) else {
                unreachable!("validated snapshots have nominal content")
            };
            ValueType::Named(nexa_bytecode::snapshot_type(content_type))
        }
        TypeRef::Snapshot(None) => unreachable!("validated snapshots are typed"),
        TypeRef::Array(inner) => {
            ValueType::Named(nexa_bytecode::array_type(value_type(idl, inner)))
        }
        TypeRef::Buffer(inner) => {
            ValueType::Named(nexa_bytecode::buffer_type(value_type(idl, inner)))
        }
        TypeRef::Option(inner) => {
            ValueType::Named(nexa_bytecode::option_type(value_type(idl, inner)).type_id)
        }
        TypeRef::Result(success, error) => ValueType::Named(
            nexa_bytecode::result_type(value_type(idl, success), value_type(idl, error)).type_id,
        ),
        TypeRef::Named(name)
            if idl.structs.iter().any(|item| item.name == *name)
                || idl.enums.iter().any(|item| item.name == *name)
                || idl.opaque_handles.contains(name) =>
        {
            ValueType::Named(StableId::from_name(name))
        }
        TypeRef::Named(_) => unreachable!("IDL named types are validated"),
    }
}

fn rust_type(ty: &TypeRef) -> String {
    match ty {
        TypeRef::I32 => "i32".into(),
        TypeRef::I64 => "i64".into(),
        TypeRef::F32 => "f32".into(),
        TypeRef::F64 => "f64".into(),
        TypeRef::Bool => "bool".into(),
        TypeRef::Rune => "char".into(),
        TypeRef::String => "String".into(),
        TypeRef::HostRequest(_) => "nexa_runtime::HostRequestHandle".into(),
        TypeRef::ResourceToken(Some(inner)) => typed_handle_name(inner, "Token"),
        TypeRef::ResourceToken(None) => unreachable!("validated tokens are typed"),
        TypeRef::Snapshot(Some(inner)) => typed_handle_name(inner, "Snapshot"),
        TypeRef::Snapshot(None) => unreachable!("validated snapshots are typed"),
        TypeRef::Array(inner) => format!("Vec<{}>", rust_type(inner)),
        TypeRef::Buffer(inner) => {
            format!("nexa_runtime::CopyBuffer<{}>", rust_type(inner))
        }
        TypeRef::Option(inner) => format!("Option<{}>", rust_type(inner)),
        TypeRef::Result(success, error) => {
            format!("Result<{}, {}>", rust_type(success), rust_type(error))
        }
        TypeRef::Named(name) => name.clone(),
    }
}

fn input_rust_type(idl: &Idl, ty: &TypeRef, lifetime: &str) -> String {
    match ty {
        TypeRef::I32 => "i32".into(),
        TypeRef::I64 => "i64".into(),
        TypeRef::F32 => "f32".into(),
        TypeRef::F64 => "f64".into(),
        TypeRef::Bool => "bool".into(),
        TypeRef::Rune => "char".into(),
        TypeRef::String => format!("&{lifetime} str"),
        TypeRef::HostRequest(_) => "nexa_runtime::HostRequestHandle".into(),
        TypeRef::ResourceToken(Some(inner)) => typed_handle_name(inner, "Token"),
        TypeRef::ResourceToken(None) => unreachable!("validated tokens are typed"),
        TypeRef::Snapshot(Some(inner)) => typed_handle_name(inner, "Snapshot"),
        TypeRef::Snapshot(None) => unreachable!("validated snapshots are typed"),
        TypeRef::Array(_) => format!("nexa_runtime::HostArrayRef<{lifetime}>"),
        TypeRef::Buffer(_) => format!("nexa_runtime::HostBufferRef<{lifetime}>"),
        TypeRef::Option(inner) => {
            format!("Option<{}>", input_rust_type(idl, inner, lifetime))
        }
        TypeRef::Result(success, error) => format!(
            "Result<{}, {}>",
            input_rust_type(idl, success, lifetime),
            input_rust_type(idl, error, lifetime)
        ),
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => name.clone(),
        TypeRef::Named(name) => format!("{name}Ref<{lifetime}>"),
    }
}

fn input_type_borrows(idl: &Idl, ty: &TypeRef) -> bool {
    match ty {
        TypeRef::String | TypeRef::Array(_) | TypeRef::Buffer(_) => true,
        TypeRef::Option(inner) => input_type_borrows(idl, inner),
        TypeRef::Result(success, error) => {
            input_type_borrows(idl, success) || input_type_borrows(idl, error)
        }
        TypeRef::Named(name) => !idl.opaque_handles.contains(name),
        TypeRef::I32
        | TypeRef::I64
        | TypeRef::F32
        | TypeRef::F64
        | TypeRef::Bool
        | TypeRef::Rune
        | TypeRef::HostRequest(_)
        | TypeRef::ResourceToken(_)
        | TypeRef::Snapshot(_) => false,
    }
}

fn tokenize(source: &str) -> Vec<String> {
    let mut output = String::new();
    for character in source.chars() {
        if character == '>' && output.ends_with('-') {
            output.push(character);
            output.push(' ');
        } else if "{}(),:;<>".contains(character) {
            output.push(' ');
            output.push(character);
            output.push(' ');
        } else if character == '-' {
            output.push(' ');
            output.push(character);
        } else {
            output.push(character);
        }
    }
    output.split_whitespace().map(str::to_owned).collect()
}

struct Parser {
    tokens: Vec<String>,
    cursor: usize,
}

impl Parser {
    #[allow(clippy::too_many_lines)]
    fn parse(&mut self) -> Result<Idl, IdlError> {
        self.expect("interface")?;
        let interface = self.word()?;
        self.expect("{")?;
        let mut idl = Idl {
            interface,
            opaque_handles: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            functions: Vec::new(),
            exports: Vec::new(),
        };
        while !self.take("}") {
            match self.peek() {
                Some("opaque") => {
                    self.cursor += 1;
                    let name = self.word()?;
                    self.expect(";")?;
                    insert_unique(&mut idl.opaque_handles, name)?;
                }
                Some("struct") => {
                    self.cursor += 1;
                    let name = self.word()?;
                    self.expect("{")?;
                    let mut fields = Vec::new();
                    while !self.take("}") {
                        fields.push(self.field()?);
                        self.expect(";")?;
                    }
                    if idl.structs.iter().any(|item| item.name == name) {
                        return Err(IdlError::Duplicate(name));
                    }
                    idl.structs.push(Struct { name, fields });
                }
                Some("enum") => {
                    self.cursor += 1;
                    let name = self.word()?;
                    self.expect("{")?;
                    let mut variants = Vec::new();
                    while !self.take("}") {
                        let variant_name = self.word()?;
                        if variants
                            .iter()
                            .any(|variant: &EnumVariant| variant.name == variant_name)
                        {
                            return Err(IdlError::Duplicate(format!("{name}::{variant_name}")));
                        }
                        let payload = if self.take("(") {
                            let payload = self.ty()?;
                            self.expect(")")?;
                            Some(payload)
                        } else {
                            None
                        };
                        variants.push(EnumVariant {
                            name: variant_name,
                            payload,
                        });
                        self.take(",");
                    }
                    if idl.enums.iter().any(|item| item.name == name) {
                        return Err(IdlError::Duplicate(name));
                    }
                    idl.enums.push(Enum { name, variants });
                }
                Some("sync" | "request") => {
                    let synchronous = self.word()? == "sync";
                    let (cancel_policy, abandon_policy) = if !synchronous && self.take("(") {
                        let cancel_policy = match self.word()?.as_str() {
                            "return_error" => CancelPolicy::ReturnError,
                            "cancel_task" => CancelPolicy::CancelTask,
                            value => {
                                return Err(IdlError::Syntax(format!(
                                    "unknown cancel policy {value}"
                                )));
                            }
                        };
                        self.expect(",")?;
                        let abandon_policy = match self.word()?.as_str() {
                            "return_error" => AbandonPolicy::ReturnError,
                            "trap" => AbandonPolicy::Trap,
                            value => {
                                return Err(IdlError::Syntax(format!(
                                    "unknown abandon policy {value}"
                                )));
                            }
                        };
                        self.expect(")")?;
                        (cancel_policy, abandon_policy)
                    } else {
                        (CancelPolicy::ReturnError, AbandonPolicy::Trap)
                    };
                    let fuel_cost = if self.take("fuel") {
                        let value = self.word()?;
                        let fuel_cost = value
                            .parse::<u32>()
                            .map_err(|_| IdlError::Syntax(format!("invalid fuel cost {value}")))?;
                        if fuel_cost == 0 {
                            return Err(IdlError::Syntax(
                                "fuel cost must be greater than zero".into(),
                            ));
                        }
                        fuel_cost
                    } else {
                        1
                    };
                    self.expect("fn")?;
                    let name = self.word()?;
                    self.expect("(")?;
                    let mut parameters = Vec::new();
                    if !self.take(")") {
                        loop {
                            parameters.push(self.field()?);
                            if self.take(")") {
                                break;
                            }
                            self.expect(",")?;
                        }
                    }
                    self.expect("->")?;
                    let result = self.ty()?;
                    self.expect(";")?;
                    if idl.functions.iter().any(|item| item.name == name) {
                        return Err(IdlError::Duplicate(name));
                    }
                    idl.functions.push(HostFunction {
                        name,
                        parameters,
                        result,
                        synchronous,
                        fuel_cost,
                        cancel_policy,
                        abandon_policy,
                    });
                }
                Some("export") => {
                    self.cursor += 1;
                    let name = self.word()?;
                    self.expect("(")?;
                    let mut parameters = Vec::new();
                    if !self.take(")") {
                        loop {
                            parameters.push(self.field()?);
                            if self.take(")") {
                                break;
                            }
                            self.expect(",")?;
                        }
                    }
                    self.expect("->")?;
                    let result = if self.take("void") {
                        None
                    } else {
                        Some(self.ty()?)
                    };
                    self.expect(";")?;
                    if idl.exports.iter().any(|item| item.name == name) {
                        return Err(IdlError::Duplicate(name));
                    }
                    idl.exports.push(Export {
                        name,
                        parameters,
                        result,
                    });
                }
                token => return Err(IdlError::Syntax(format!("unexpected {token:?}"))),
            }
        }
        validate_types(&idl)?;
        Ok(idl)
    }

    fn field(&mut self) -> Result<Field, IdlError> {
        let name = self.word()?;
        self.expect(":")?;
        Ok(Field {
            name,
            ty: self.ty()?,
        })
    }

    fn ty(&mut self) -> Result<TypeRef, IdlError> {
        let name = self.word()?;
        Ok(match name.as_str() {
            "i32" => TypeRef::I32,
            "i64" => TypeRef::I64,
            "f32" => TypeRef::F32,
            "f64" => TypeRef::F64,
            "bool" => TypeRef::Bool,
            "rune" => TypeRef::Rune,
            "string" => TypeRef::String,
            "host_request" | "request" => TypeRef::HostRequest(self.optional_type_argument()?),
            "resource_token" | "token" => TypeRef::ResourceToken(self.optional_type_argument()?),
            "snapshot" => TypeRef::Snapshot(self.optional_type_argument()?),
            "array" => {
                self.expect("<")?;
                let inner = self.ty()?;
                self.expect(">")?;
                TypeRef::Array(Box::new(inner))
            }
            "buffer" => {
                self.expect("<")?;
                let inner = self.ty()?;
                self.expect(">")?;
                TypeRef::Buffer(Box::new(inner))
            }
            "Option" => {
                self.expect("<")?;
                let inner = self.ty()?;
                self.expect(">")?;
                TypeRef::Option(Box::new(inner))
            }
            "Result" => {
                self.expect("<")?;
                let success = self.ty()?;
                self.expect(",")?;
                let error = self.ty()?;
                self.expect(">")?;
                TypeRef::Result(Box::new(success), Box::new(error))
            }
            named if self.peek() != Some("<") => TypeRef::Named(named.to_owned()),
            named => return Err(IdlError::UnknownType(named.to_owned())),
        })
    }

    fn optional_type_argument(&mut self) -> Result<Option<Box<TypeRef>>, IdlError> {
        if self.take("<") {
            let inner = self.ty()?;
            self.expect(">")?;
            Ok(Some(Box::new(inner)))
        } else {
            Ok(None)
        }
    }

    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.cursor).map(String::as_str)
    }

    fn take(&mut self, token: &str) -> bool {
        if self.peek() == Some(token) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &str) -> Result<(), IdlError> {
        if self.take(token) {
            Ok(())
        } else {
            Err(IdlError::Syntax(format!(
                "expected `{token}`, found {:?}",
                self.peek()
            )))
        }
    }

    fn word(&mut self) -> Result<String, IdlError> {
        let word = self
            .tokens
            .get(self.cursor)
            .ok_or_else(|| IdlError::Syntax("unexpected end".into()))?
            .clone();
        self.cursor += 1;
        Ok(word)
    }
}

fn insert_unique(items: &mut Vec<String>, name: String) -> Result<(), IdlError> {
    if items.contains(&name) {
        Err(IdlError::Duplicate(name))
    } else {
        items.push(name);
        Ok(())
    }
}

fn validate_types(idl: &Idl) -> Result<(), IdlError> {
    let known = |name: &str| {
        idl.opaque_handles.iter().any(|item| item == name)
            || idl.structs.iter().any(|item| item.name == name)
            || idl.enums.iter().any(|item| item.name == name)
    };
    for ty in idl
        .structs
        .iter()
        .flat_map(|structure| structure.fields.iter().map(|field| &field.ty))
        .chain(idl.enums.iter().flat_map(|enumeration| {
            enumeration
                .variants
                .iter()
                .filter_map(|variant| variant.payload.as_ref())
        }))
        .chain(idl.functions.iter().flat_map(|function| {
            function
                .parameters
                .iter()
                .map(|field| &field.ty)
                .chain(std::iter::once(&function.result))
        }))
        .chain(idl.exports.iter().flat_map(|export| {
            export
                .parameters
                .iter()
                .map(|field| &field.ty)
                .chain(export.result.iter())
        }))
    {
        validate_type_ref(ty, &known)?;
    }
    if idl.functions.iter().any(|function| {
        !function.synchronous
            && !matches!(
                function.result,
                TypeRef::HostRequest(Some(ref inner))
                    if matches!(inner.as_ref(), TypeRef::Result(_, _))
            )
    }) {
        return Err(IdlError::Syntax(
            "request functions must return request<Result<Success, Error>>".into(),
        ));
    }
    if idl
        .functions
        .iter()
        .any(|function| function.synchronous && matches!(function.result, TypeRef::HostRequest(_)))
    {
        return Err(IdlError::Syntax(
            "sync functions cannot return request values".into(),
        ));
    }
    Ok(())
}

fn validate_type_ref(ty: &TypeRef, known: &impl Fn(&str) -> bool) -> Result<(), IdlError> {
    match ty {
        TypeRef::Named(name) if !known(name) => Err(IdlError::UnknownType(name.clone())),
        TypeRef::HostRequest(None) => Err(IdlError::Syntax(
            "request types require a result type".into(),
        )),
        TypeRef::ResourceToken(None) => Err(IdlError::Syntax(
            "token types require a resource domain".into(),
        )),
        TypeRef::Snapshot(None) => Err(IdlError::Syntax(
            "snapshot types require a nominal content type".into(),
        )),
        TypeRef::Snapshot(Some(inner)) if !matches!(inner.as_ref(), TypeRef::Named(_)) => Err(
            IdlError::Syntax("snapshot content types must be nominal".into()),
        ),
        TypeRef::ResourceToken(Some(inner)) if !matches!(inner.as_ref(), TypeRef::Named(_)) => Err(
            IdlError::Syntax("token resource domains must be nominal".into()),
        ),
        TypeRef::HostRequest(Some(inner))
        | TypeRef::ResourceToken(Some(inner))
        | TypeRef::Snapshot(Some(inner))
        | TypeRef::Array(inner)
        | TypeRef::Buffer(inner)
        | TypeRef::Option(inner) => validate_type_ref(inner, known),
        TypeRef::Result(success, error) => {
            validate_type_ref(success, known)?;
            validate_type_ref(error, known)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        AbandonPolicy, CancelPolicy, Export, Field, TypeRef, exact_hash, generate_rust, parse,
    };

    const IDL: &str = "
        interface GameHost {
            opaque Entity;
            struct Vec3 { x: f32; y: f32; z: f32; }
            sync fn entity_position(entity: Entity) -> Vec3;
            export Update(entity: Entity, dt: f32) -> void;
        }
    ";

    #[test]
    fn exact_hash_ignores_comments_and_whitespace_but_preserves_field_order() {
        let first = parse(IDL).unwrap();
        let second = parse(&format!("// comment\n{IDL}")).unwrap();
        assert_eq!(exact_hash(&first), exact_hash(&second));
        let reordered = parse(&IDL.replace("x: f32; y: f32", "y: f32; x: f32")).unwrap();
        assert_ne!(exact_hash(&first), exact_hash(&reordered));
        let payload_i32 = parse("interface Events { enum Event { Idle, Damage(i32) } }").unwrap();
        let payload_i64 = parse("interface Events { enum Event { Idle, Damage(i64) } }").unwrap();
        assert_ne!(exact_hash(&payload_i32), exact_hash(&payload_i64));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn twenty_interface_changes_regenerate_deterministically_and_change_exact_hash() {
        let base = parse(
            "interface Exact {
                opaque DomainA;
                opaque DomainB;
                struct Payload { value: i32; }
                enum Failure { Cancelled, Invalid(i32) }
                sync fuel 7 fn send(
                    data: buffer<Payload>,
                    view: snapshot<Payload>,
                    lease: token<DomainA>
                ) -> i32;
            }",
        )
        .unwrap();
        let base_hash = exact_hash(&base);
        let changed = |idl| assert_ne!(base_hash, exact_hash(&idl));

        let mut idl = base.clone();
        idl.functions[0].name = "renamed".into();
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].parameters[0].ty = TypeRef::Buffer(Box::new(TypeRef::I64));
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].result = TypeRef::I64;
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].synchronous = false;
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].fuel_cost = 8;
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].cancel_policy = CancelPolicy::CancelTask;
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].abandon_policy = AbandonPolicy::ReturnError;
        changed(idl);
        let mut idl = base.clone();
        idl.enums[0].variants[0].name = "Stopped".into();
        changed(idl);
        let mut idl = base.clone();
        idl.structs[0].fields[0].name = "other".into();
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].parameters[1].ty =
            TypeRef::Snapshot(Some(Box::new(TypeRef::Named("DomainA".into()))));
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].parameters[2].ty =
            TypeRef::ResourceToken(Some(Box::new(TypeRef::Named("DomainB".into()))));
        changed(idl);
        let mut idl = base.clone();
        idl.interface = "ExactRenamed".into();
        changed(idl);
        let mut idl = base.clone();
        idl.opaque_handles[0] = "DomainRenamed".into();
        idl.functions[0].parameters[2].ty =
            TypeRef::ResourceToken(Some(Box::new(TypeRef::Named("DomainRenamed".into()))));
        changed(idl);
        let mut idl = base.clone();
        idl.structs[0].name = "PayloadRenamed".into();
        idl.functions[0].parameters[0].ty =
            TypeRef::Buffer(Box::new(TypeRef::Named("PayloadRenamed".into())));
        idl.functions[0].parameters[1].ty =
            TypeRef::Snapshot(Some(Box::new(TypeRef::Named("PayloadRenamed".into()))));
        changed(idl);
        let mut idl = base.clone();
        idl.enums[0].name = "FailureRenamed".into();
        changed(idl);
        let mut idl = base.clone();
        idl.enums[0].variants[1].payload = Some(TypeRef::I64);
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].parameters[0].name = "payload".into();
        changed(idl);
        let mut idl = base.clone();
        idl.functions[0].parameters.push(Field {
            name: "attempt".into(),
            ty: TypeRef::I32,
        });
        changed(idl);
        let mut idl = base.clone();
        idl.structs[0].fields.push(Field {
            name: "extra".into(),
            ty: TypeRef::Bool,
        });
        changed(idl);
        let mut idl = base.clone();
        idl.exports.push(Export {
            name: "Tick".into(),
            parameters: vec![Field {
                name: "frame".into(),
                ty: TypeRef::I64,
            }],
            result: Some(TypeRef::Bool),
        });
        changed(idl);

        assert_eq!(generate_rust(&base), generate_rust(&base));
        let generated = generate_rust(&base);
        assert!(generated.contains("pub const INTERFACE_HASH"));
        assert!(generated.contains("pub const FUNCTION_ID_SEND"));
        assert!(generated.contains("pub const NEXA_MODULE_DECLARATION"));
        assert!(generated.contains("pub struct GeneratedHostStub"));
    }

    #[test]
    fn fuel_cost_is_explicitly_parsed_and_validated() {
        let idl = parse("interface Fuel { sync fuel 9 fn work() -> i32; }").unwrap();
        assert_eq!(idl.functions[0].fuel_cost, 9);
        assert!(parse("interface Fuel { sync fuel 0 fn work() -> i32; }").is_err());
        assert!(parse("interface Fuel { sync fuel many fn work() -> i32; }").is_err());
    }

    #[test]
    fn binding_has_typed_trait_thunk_and_export_marker() {
        let generated = generate_rust(&parse(IDL).unwrap());
        assert!(generated.contains("pub trait GameHost"));
        assert!(generated.contains("entity_position"));
        assert!(generated.contains("THUNK_ENTITY_POSITION"));
        assert!(generated.contains("pub enum Update"));
        assert!(generated.contains("pub const EXPORT_ID"));
        assert!(generated.contains("pub struct UpdateArgs"));
    }

    #[test]
    fn generated_runtime_thunks_use_borrowed_inputs() {
        let generated = generate_rust(
            &parse(
                "interface GameHost {
                    struct CombatStats { label: string; health: i32; }
                    enum CombatEvent { Idle, Changed(CombatStats) }
                    sync fn process(
                        name: string,
                        stats: CombatStats,
                        event: CombatEvent,
                        values: array<i32>,
                        data: buffer<i32>,
                        option: Option<CombatStats>
                    ) -> i32;
                }",
            )
            .unwrap(),
        );
        let trait_start = generated.find("pub trait GameHost").unwrap();
        let trait_end = generated[trait_start..].find("}\n").unwrap() + trait_start;
        let host_trait = &generated[trait_start..=trait_end];
        for borrowed in [
            "name: &'a str",
            "stats: CombatStatsRef<'a>",
            "event: CombatEventRef<'a>",
            "values: nexa_runtime::HostArrayRef<'a>",
            "data: nexa_runtime::HostBufferRef<'a>",
            "option: Option<CombatStatsRef<'a>>",
        ] {
            assert!(host_trait.contains(borrowed), "{borrowed}\n{host_trait}");
        }
        for owning in [
            "name: String",
            "stats: CombatStats,",
            "event: CombatEvent,",
            "values: Vec<",
            "data: nexa_runtime::CopyBuffer",
            "option: Option<CombatStats>",
        ] {
            assert!(!host_trait.contains(owning), "{owning}\n{host_trait}");
        }
    }

    #[test]
    fn generated_runtime_thunks_decode_complex_inputs_directly() {
        let generated = generate_rust(
            &parse(
                "interface Direct {
                    struct Record { name: string; value: i32; }
                    enum Event { Empty, Record(Record) }
                    sync fn decode(
                        name: string,
                        record: Record,
                        event: Event,
                        option: Option<Record>,
                        result: Result<Record, Event>,
                        array: array<Record>,
                        buffer: buffer<Record>,
                        nested: Option<Result<Event, Record>>
                    ) -> Result<Record, Event>;
                }",
            )
            .unwrap(),
        );
        let runtime_start = generated.find("fn call_runtime").unwrap();
        let runtime_end = generated[runtime_start..]
            .find("pub fn registry")
            .map(|offset| runtime_start + offset)
            .unwrap();
        let runtime_dispatch = &generated[runtime_start..runtime_end];
        for direct in [
            "args.str_ref(0)?.as_str()",
            "args.value_ref(1)?",
            ".struct_ref(",
            ".enum_ref(",
            ".array_ref(",
            ".buffer_ref(",
        ] {
            assert!(
                runtime_dispatch.contains(direct),
                "{direct}\n{runtime_dispatch}"
            );
        }
        for forbidden in [
            "args.host_value(",
            "runtime_argument_to_host_value",
            "to_owned",
            "collect::<Vec",
            "Box::new",
            "CopyBuffer::new",
        ] {
            assert!(
                !runtime_dispatch.contains(forbidden),
                "{forbidden}\n{runtime_dispatch}"
            );
        }
    }

    #[test]
    fn generated_return_encoders_stream_non_empty_nested_collections() {
        let generated = generate_rust(
            &parse(
                "interface Returns {
                    struct Record { label: string; value: i32; }
                    enum Event { Empty, Record(Record) }
                    sync fn array_struct() -> array<Record>;
                    sync fn buffer_struct() -> buffer<Record>;
                    sync fn nested() -> Option<array<Event>>;
                    sync fn result() -> Result<buffer<Record>, Event>;
                }",
            )
            .unwrap(),
        );
        let return_start = generated
            .find("impl nexa_runtime::EncodeHostReturn")
            .unwrap();
        let runtime_start = generated.find("fn call_runtime").unwrap();
        let return_paths = &generated[return_start..];
        let runtime_paths = &generated[runtime_start..];
        for required in [
            "HostReturnRequirements",
            ".checked_add(",
            ".begin_array(",
            ".push_array_value(",
            ".finish_array(",
            ".begin_buffer(",
            ".push_buffer_value(",
            ".finish_buffer(",
            ".return_transaction(",
            ".commit(",
        ] {
            assert!(
                return_paths.contains(required) || runtime_paths.contains(required),
                "{required}\n{generated}"
            );
        }
        for forbidden in [
            "Vec::with_capacity",
            "collect::<Vec",
            "into_vec()",
            "args.host_value",
        ] {
            assert!(
                !runtime_paths.contains(forbidden),
                "{forbidden}\n{runtime_paths}"
            );
        }
    }

    #[test]
    fn generated_types_are_deterministic_and_cover_typed_host_handles() {
        let idl = parse(
            "interface Typed {
                opaque ActionLock;
                struct EnemyView { health: i32; }
                enum Failure { Cancelled }
                sync fn lock() -> token<ActionLock>;
                sync fn view() -> snapshot<EnemyView>;
                request(return_error, trap) fn load(values: buffer<i32>)
                    -> request<Result<buffer<EnemyView>, Failure>>;
                export Update(view: snapshot<EnemyView>) -> buffer<i32>;
            }",
        )
        .unwrap();
        let first = generate_rust(&idl);
        let second = generate_rust(&idl);
        assert_eq!(first, second);
        for expected in [
            "pub struct ActionLockToken",
            "pub struct EnemyViewSnapshot",
            "impl TryFrom<nexa_runtime::SnapshotHandle> for EnemyViewSnapshot",
            "pub trait Typed",
            "pub struct LoadCompletionTicket",
            "nexa_runtime::CopyBuffer<EnemyView>",
            "impl nexa_runtime::ScriptExport for Update",
            "pub const EXPORT_ID",
        ] {
            assert!(first.contains(expected), "{expected}");
        }
    }

    #[test]
    fn typed_snapshot_codec() {
        let generated = generate_rust(
            &parse(
                "interface Snapshots {
                    struct Position { x: i32; y: i32; }
                    enum Status { Idle, Named(string) }
                    struct EnemyView {
                        health: i32;
                        name: string;
                        status: Status;
                        samples: array<i32>;
                        position: Position;
                    }
                    sync fn view() -> snapshot<EnemyView>;
                }",
            )
            .unwrap(),
        );
        for required in [
            "pub struct EnemyViewSnapshotEncoder",
            "pub struct EnemyViewSnapshotRef<'a>",
            "impl<'a> nexa_runtime::DecodeTypedSnapshot<'a>",
            "pub const SCHEMA_HASH",
            "EncodedSnapshot::new",
            "decode_owned",
            "std::str::from_utf8",
            "Vec::with_capacity",
            "Status::Named",
            "Position {",
        ] {
            assert!(generated.contains(required), "{required}\n{generated}");
        }
    }

    #[test]
    fn parses_the_complete_internal_boundary_type_matrix() {
        let idl = parse(
            "interface Complete {
                opaque Entity;
                opaque ActionLock;
                struct Packet {
                    i: i32; wide: i64; x: f32; y: f64; flag: bool; glyph: rune;
                    label: string; maybe: Option<Entity>; values: array<i32>;
                    bytes: buffer<i32>; view: snapshot<Entity>; lock: token<ActionLock>;
                }
                enum Event { Idle, Packet(Packet) }
                sync fn roundtrip(value: Packet) -> Result<Event, i32>;
                request(return_error, trap) fn load(value: array<Packet>)
                    -> request<Result<buffer<Event>, i32>>;
            }",
        )
        .unwrap();
        let packet = &idl.structs[0];
        assert!(matches!(packet.fields[0].ty, TypeRef::I32));
        assert!(matches!(packet.fields[1].ty, TypeRef::I64));
        assert!(matches!(packet.fields[2].ty, TypeRef::F32));
        assert!(matches!(packet.fields[3].ty, TypeRef::F64));
        assert!(matches!(packet.fields[4].ty, TypeRef::Bool));
        assert!(matches!(packet.fields[5].ty, TypeRef::Rune));
        assert!(matches!(packet.fields[6].ty, TypeRef::String));
        assert!(matches!(packet.fields[7].ty, TypeRef::Option(_)));
        assert!(matches!(packet.fields[8].ty, TypeRef::Array(_)));
        assert!(matches!(packet.fields[9].ty, TypeRef::Buffer(_)));
        assert!(matches!(packet.fields[10].ty, TypeRef::Snapshot(_)));
        assert!(matches!(packet.fields[11].ty, TypeRef::ResourceToken(_)));
        assert!(matches!(
            idl.enums[0].variants[1].payload,
            Some(TypeRef::Named(_))
        ));
        assert!(matches!(idl.functions[0].result, TypeRef::Result(_, _)));
        assert!(matches!(
            idl.functions[1].result,
            TypeRef::HostRequest(Some(_))
        ));
    }

    #[test]
    fn rejects_untyped_resource_and_request_handles() {
        for source in [
            "interface Bad { sync fn load() -> request; }",
            "interface Bad { sync fn lock() -> token; }",
            "interface Bad { sync fn view() -> snapshot; }",
            "interface Bad { sync fn view() -> snapshot<i32>; }",
        ] {
            assert!(parse(source).is_err(), "{source}");
        }
    }

    #[test]
    fn typed_request_hashes_policies_and_generates_completion_wrapper() {
        let typed = parse(
            "interface Engine {
                enum LoadError { Missing, Cancelled }
                request(return_error, trap) fn load()
                    -> request<Result<i32, LoadError>>;
            }",
        )
        .unwrap();
        let generated = generate_rust(&typed);
        assert!(generated.contains("pub enum LoadError"));
        assert!(generated.contains("pub struct LoadCompletionTicket"));
        assert!(generated.contains("Result<i32, LoadError>"));

        let cancel_task = parse(
            "interface Engine {
                enum LoadError { Missing, Cancelled }
                request(cancel_task, trap) fn load()
                    -> request<Result<i32, LoadError>>;
            }",
        )
        .unwrap();
        assert_ne!(exact_hash(&typed), exact_hash(&cancel_task));

        let scalar = generate_rust(
            &parse(
                "interface Scalars {
                    request(return_error, trap) fn sample()
                        -> request<Result<f64, i32>>;
                }",
            )
            .unwrap(),
        );
        assert!(scalar.contains("HostPayload::F64(value.to_bits())"));

        let enum_payload = generate_rust(
            &parse(
                "interface Events {
                    enum Event { Idle, Damage(i32) }
                    enum EventError { Cancelled }
                    request(return_error, trap) fn next()
                        -> request<Result<Event, EventError>>;
                }",
            )
            .unwrap(),
        );
        assert!(enum_payload.contains("HostPayload::Enum"));
        assert!(enum_payload.contains("payload: Some(Box::new"));

        let struct_payload = generate_rust(
            &parse(
                "interface Geometry {
                    struct Position { x: i32; label: string; }
                    enum GeometryError { Cancelled }
                    request(return_error, trap) fn position()
                        -> request<Result<Position, GeometryError>>;
                }",
            )
            .unwrap(),
        );
        assert!(struct_payload.contains("HostPayload::structure"));
        assert!(struct_payload.contains("HostPayload::String(value.label)"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn generated_registry_compiles_and_executes_a_mock_host() {
        let idl = parse(
            "interface GameHost {
                opaque Entity;
                struct Vec3 { x: f32; y: f32; z: f32; }
                enum Event { Idle, Damage(i32), Label(string) }
                enum EventError { Cancelled }
                sync fn add(lhs: i32, rhs: i32) -> i32;
                sync fn position(entity: Entity, value: Vec3) -> Vec3;
                sync fn scalar_mix(wide: i64, ratio: f64, glyph: rune) -> f64;
                sync fn echo(value: string) -> string;
                sync fn echo_event(value: Event) -> Event;
                sync fn collections(values: array<Option<i32>>, copy: buffer<i64>)
                    -> array<Result<i32, i32>>;
                sync fn sum8(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32, g: i32, h: i32)
                    -> i32;
                sync fn explode() -> i32;
                request(return_error, trap) fn next()
                    -> request<Result<Event, EventError>>;
                export Update(value: i32) -> i32;
            }",
        )
        .unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("nexa-idl-generated-{}-{nonce}", std::process::id()));
        fs::create_dir_all(directory.join("src")).unwrap();
        let runtime = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../nexa-runtime")
            .canonicalize()
            .unwrap();
        fs::write(
            directory.join("Cargo.toml"),
            format!(
                "[package]\nname=\"generated-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n\
                 [dependencies]\nnexa-runtime={{path={runtime:?}}}\n"
            ),
        )
        .unwrap();
        let generated = generate_rust(&idl);
        assert!(generated.contains("fn call_runtime"));
        assert!(generated.contains("args.i32(7)?"));
        fs::write(directory.join("src/bindings.rs"), generated).unwrap();
        fs::write(
            directory.join("src/main.rs"),
            r#"mod bindings;
use bindings::{Entity, Event, EventRef, GameHost, GeneratedHostRegistry, HostError, Vec3, Vec3Ref};
use nexa_runtime::{HostCallOutcome, HostRegistry, RuntimeLimits, RuntimeResources, TaskRuntime};

struct Mock;
impl GameHost for Mock {
    fn add(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        lhs: i32,
        rhs: i32,
    ) -> Result<i32, HostError> {
        Ok(lhs + rhs)
    }

    fn position<'a>(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        entity: Entity,
        value: Vec3Ref<'a>,
    ) -> Result<Vec3, HostError> {
        Ok(Vec3 {
            x: value.x().map_err(|error| HostError(format!("{error:?}")))? + entity.0 as f32,
            y: value.y().map_err(|error| HostError(format!("{error:?}")))?,
            z: value.z().map_err(|error| HostError(format!("{error:?}")))?,
        })
    }

    fn scalar_mix(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        wide: i64,
        ratio: f64,
        glyph: char,
    ) -> Result<f64, HostError> {
        Ok(wide as f64 + ratio + f64::from(glyph as u32))
    }

    fn echo<'a>(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        value: &'a str,
    ) -> Result<String, HostError> {
        Ok(value.to_owned())
    }

    fn echo_event<'a>(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        value: EventRef<'a>,
    ) -> Result<Event, HostError> {
        Ok(match value {
            EventRef::Idle => Event::Idle,
            EventRef::Damage(value) => Event::Damage(value),
            EventRef::Label(value) => Event::Label(value.to_owned()),
            EventRef::__Lifetime(_) => unreachable!(),
        })
    }

    fn collections<'a>(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        values: nexa_runtime::HostArrayRef<'a>,
        copy: nexa_runtime::HostBufferRef<'a>,
    ) -> Result<Vec<Result<i32, i32>>, HostError> {
        let offset = if copy.is_empty() {
            0
        } else {
            copy.get(0).map_err(|error| HostError(format!("{error:?}")))?
                .i64().map_err(|error| HostError(format!("{error:?}")))? as i32
        };
        Ok((0..values.len()).map(|_| Err(offset)).collect())
    }

    fn explode(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
    ) -> Result<i32, HostError> {
        panic!("host panic")
    }

    fn sum8(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        a: i32,
        b: i32,
        c: i32,
        d: i32,
        e: i32,
        f: i32,
        g: i32,
        h: i32,
    ) -> Result<i32, HostError> {
        Ok(a + b + c + d + e + f + g + h)
    }

    fn next(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
    ) -> Result<nexa_runtime::HostRequestHandle, HostError> {
        context
            .create_request()
            .map(|pending| pending.request)
            .map_err(|error| HostError(error.to_string()))
    }
}

fn main() {
    let mut tasks = TaskRuntime::new(1, RuntimeLimits::default());
    let scope = tasks.create_scope(None).unwrap();
    let task = tasks.admit_task(scope, 1, true).unwrap();
    let mut resources = RuntimeResources::new(1, 4, 8);
    let mut context = resources.context(task, 0, 1);
    let mut registry = GeneratedHostRegistry::new(Mock);
    assert_eq!(
        registry
            .call_runtime(
                6,
                &mut context,
                nexa_runtime::RuntimeHostArgs::new(
                    &[
                        nexa_runtime::RuntimeValue::I32(1),
                        nexa_runtime::RuntimeValue::I32(2),
                        nexa_runtime::RuntimeValue::I32(3),
                        nexa_runtime::RuntimeValue::I32(4),
                        nexa_runtime::RuntimeValue::I32(5),
                        nexa_runtime::RuntimeValue::I32(6),
                        nexa_runtime::RuntimeValue::I32(7),
                        nexa_runtime::RuntimeValue::I32(8),
                    ],
                    None,
                )
                .unwrap(),
            )
            .unwrap(),
        HostCallOutcome::RuntimeImmediate(nexa_runtime::RuntimeValue::I32(36))
    );
}
"#,
        )
        .unwrap();
        let output = Command::new("cargo")
            .args(["+1.97.1", "run", "--quiet"])
            .current_dir(&directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "generated crate failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
