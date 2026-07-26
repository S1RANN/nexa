//! Exact-build IDL parsing, canonical hashing and Rust binding generation.

use std::fmt;
use std::fmt::Write;

use nexa_bytecode::ValueType;
use nexa_core::StableId;

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
pub fn canonical(idl: &Idl) -> String {
    let mut output = format!("interface:{};", idl.interface);
    for handle in &idl.opaque_handles {
        write!(output, "opaque:{handle};").expect("String writes do not fail");
    }
    for structure in &idl.structs {
        write!(output, "struct:{}{{", structure.name).expect("String writes do not fail");
        for field in &structure.fields {
            write!(output, "{}:{};", field.name, type_name(&field.ty))
                .expect("String writes do not fail");
        }
        output.push('}');
    }
    for enumeration in &idl.enums {
        write!(output, "enum:{}{{", enumeration.name).expect("String writes do not fail");
        for variant in &enumeration.variants {
            write!(output, "{}", variant.name).expect("String writes do not fail");
            if let Some(payload) = &variant.payload {
                write!(output, "({})", type_name(payload)).expect("String writes do not fail");
            }
            output.push(';');
        }
        output.push('}');
    }
    for function in &idl.functions {
        write!(
            output,
            "fn:{}:{}:{}:{}(",
            if function.synchronous {
                "sync"
            } else {
                "request"
            },
            function.name,
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
            write!(output, "{}:{};", parameter.name, type_name(&parameter.ty))
                .expect("String writes do not fail");
        }
        write!(output, ")->{};", type_name(&function.result)).expect("String writes do not fail");
    }
    for export in &idl.exports {
        write!(output, "export:{}(", export.name).expect("String writes do not fail");
        for parameter in &export.parameters {
            write!(output, "{}:{};", parameter.name, type_name(&parameter.ty))
                .expect("String writes do not fail");
        }
        if let Some(result) = &export.result {
            write!(output, ")->{};", type_name(result)).expect("String writes do not fail");
        } else {
            output.push_str(")->void;");
        }
    }
    output
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn generate_rust(idl: &Idl) -> String {
    let mut output = String::from(
        "// @generated by nexa-idl; do not edit.\n\
         #[derive(Debug)] pub struct HostError(pub String);\n",
    );
    for handle in &idl.opaque_handles {
        writeln!(
            output,
            "#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)] pub struct {handle}(pub u64);"
        )
        .expect("String writes do not fail");
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
    }
    writeln!(output, "pub trait {} {{", idl.interface).expect("String writes do not fail");
    for function in &idl.functions {
        write!(
            output,
            "    fn {}(&mut self, context: &mut nexa_runtime::ResourceContext<'_>",
            function.name
        )
        .expect("String writes do not fail");
        for parameter in &function.parameters {
            write!(output, ", {}: {}", parameter.name, rust_type(&parameter.ty))
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
    for (index, function) in idl.functions.iter().enumerate() {
        writeln!(
            output,
            "pub const THUNK_{}: u32 = {index};",
            function.name.to_ascii_uppercase()
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
        "fn call(&mut self, id: u32, context: &mut nexa_runtime::ResourceContext<'_>, \
         args: nexa_runtime::HostArgs<'_>) -> Result<nexa_runtime::HostCallOutcome, nexa_runtime::HostTrap> {\n\
         match id {\n",
    );
    for (index, function) in idl.functions.iter().enumerate() {
        if function.parameters.is_empty() {
            writeln!(
                output,
                "{index} => {{ if !args.is_empty() {{ return Err(nexa_runtime::HostTrap::Arity); }}"
            )
            .expect("String writes do not fail");
        } else {
            writeln!(
                output,
                "{index} => {{ if args.len() != {} {{ return Err(nexa_runtime::HostTrap::Arity); }}",
                function.parameters.len()
            )
            .expect("String writes do not fail");
        }
        for (argument, parameter) in function.parameters.iter().enumerate() {
            writeln!(
                output,
                "let {} = {};",
                parameter.name,
                decode_host_value(idl, &parameter.ty, argument)
            )
            .expect("String writes do not fail");
        }
        write!(
            output,
            "let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.host.{}(context",
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
        writeln!(
            output,
            "Ok({}) }}",
            encode_host_result(idl, &function.result)
        )
        .expect("String writes do not fail");
    }
    output.push_str(
        "_ => Err(nexa_runtime::HostTrap::UnknownFunction(id)),\n\
         }\n}\n}\n",
    );
    for (index, export) in idl.exports.iter().enumerate() {
        let args = if export.parameters.is_empty() {
            format!("pub struct {}Args;", export.name)
        } else {
            let fields = export
                .parameters
                .iter()
                .map(|field| format!("pub {}: {}", field.name, rust_type(&field.ty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("pub struct {}Args {{ {fields} }}", export.name)
        };
        let output_type = export.result.as_ref().map_or("()".to_owned(), rust_type);
        writeln!(
            output,
            "{args}\n\
             pub type {name}Output = {output_type};\n\
             pub enum {name} {{}}\n\
             impl nexa_runtime::ScriptFunction for {name} {{\n\
             type Args = {name}Args; type Output = {name}Output;\n\
             const FUNCTION_ID: u32 = {index}; }}\n\
             impl {name} {{ pub const EXPORT_NAME: &'static str = \"{name}\"; }}",
            name = export.name,
        )
        .expect("String writes do not fail");
    }
    output
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
        TypeRef::ResourceToken(_) => format!("nexa_runtime::HostPayload::Token({source})"),
        TypeRef::Snapshot(_) => format!("nexa_runtime::HostPayload::Snapshot({source})"),
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
            format!("nexa_runtime::HostPayload::Struct(vec![{fields}])")
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

fn decode_host_value(idl: &Idl, ty: &TypeRef, index: usize) -> String {
    decode_value(idl, ty, &format!("args.get({index})?"))
}

fn decode_value(idl: &Idl, ty: &TypeRef, source: &str) -> String {
    match ty {
        TypeRef::I32 => decode_match(source, "I32", "*value"),
        TypeRef::I64 => decode_match(source, "I64", "*value"),
        TypeRef::F32 => decode_match(source, "F32", "*value"),
        TypeRef::F64 => decode_match(source, "F64", "*value"),
        TypeRef::Bool => decode_match(source, "Bool", "*value"),
        TypeRef::Rune => decode_match(source, "Rune", "*value"),
        TypeRef::String => decode_match(source, "String", "value.clone()"),
        TypeRef::HostRequest(_) => decode_match(source, "Request", "*value"),
        TypeRef::ResourceToken(_) => decode_match(source, "Token", "*value"),
        TypeRef::Snapshot(_) => decode_match(source, "Snapshot", "*value"),
        TypeRef::Array(inner) => decode_collection(idl, inner, source, "Array", false),
        TypeRef::Buffer(inner) => decode_collection(idl, inner, source, "Buffer", true),
        TypeRef::Option(inner) => decode_option(idl, inner, source),
        TypeRef::Result(success, error) => decode_result(idl, success, error, source),
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => format!(
            "match {source} {{ nexa_runtime::HostValue::Opaque(value) => {name}(*value), \
             _ => return Err(nexa_runtime::HostTrap::Type) }}"
        ),
        TypeRef::Named(name)
            if idl
                .enums
                .iter()
                .any(|enumeration| enumeration.name == *name) =>
        {
            decode_enum_value(
                idl,
                idl.enums
                    .iter()
                    .find(|enumeration| enumeration.name == *name)
                    .expect("validated enum exists"),
                source,
            )
        }
        TypeRef::Named(name) => {
            let structure = idl
                .structs
                .iter()
                .find(|structure| structure.name == *name)
                .expect("validated named type exists");
            let fields = structure
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| {
                    format!(
                        "{}: {}",
                        field.name,
                        decode_value(idl, &field.ty, &format!("&values[{index}]"))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "match {source} {{ nexa_runtime::HostValue::Struct(values) if values.len() == {} \
                 => {name} {{ {fields} }}, _ => return Err(nexa_runtime::HostTrap::Type) }}",
                structure.fields.len()
            )
        }
    }
}

fn decode_match(source: &str, variant: &str, value: &str) -> String {
    format!(
        "match {source} {{ nexa_runtime::HostValue::{variant}(value) => {value}, \
         _ => return Err(nexa_runtime::HostTrap::Type) }}"
    )
}

fn encode_host_result(idl: &Idl, ty: &TypeRef) -> String {
    if matches!(ty, TypeRef::HostRequest(_)) {
        "nexa_runtime::HostCallOutcome::Pending(result)".into()
    } else {
        format!(
            "nexa_runtime::HostCallOutcome::Immediate({})",
            encode_value(idl, ty, "result")
        )
    }
}

fn encode_value(idl: &Idl, ty: &TypeRef, source: &str) -> String {
    match ty {
        TypeRef::I32 => format!("nexa_runtime::HostValue::I32({source})"),
        TypeRef::I64 => format!("nexa_runtime::HostValue::I64({source})"),
        TypeRef::F32 => format!("nexa_runtime::HostValue::F32({source})"),
        TypeRef::F64 => format!("nexa_runtime::HostValue::F64({source})"),
        TypeRef::Bool => format!("nexa_runtime::HostValue::Bool({source})"),
        TypeRef::Rune => format!("nexa_runtime::HostValue::Rune({source})"),
        TypeRef::String => format!("nexa_runtime::HostValue::String({source})"),
        TypeRef::HostRequest(_) => format!("nexa_runtime::HostValue::Request({source})"),
        TypeRef::ResourceToken(_) => format!("nexa_runtime::HostValue::Token({source})"),
        TypeRef::Snapshot(_) => format!("nexa_runtime::HostValue::Snapshot({source})"),
        TypeRef::Array(inner) => format!(
            "nexa_runtime::HostValue::Array(nexa_runtime::CopyBuffer::new({source}.into_iter()\
             .map(|value| {}).collect()))",
            encode_value(idl, inner, "value")
        ),
        TypeRef::Buffer(inner) => format!(
            "nexa_runtime::HostValue::Buffer(nexa_runtime::CopyBuffer::new({source}.into_vec()\
             .into_iter().map(|value| {}).collect()))",
            encode_value(idl, inner, "value")
        ),
        TypeRef::Option(inner) => {
            encode_option(idl, inner, source, "nexa_runtime::HostValue", encode_value)
        }
        TypeRef::Result(success, error) => encode_result(
            idl,
            success,
            error,
            source,
            "nexa_runtime::HostValue",
            encode_value,
        ),
        TypeRef::Named(name) if idl.opaque_handles.contains(name) => {
            format!("nexa_runtime::HostValue::Opaque({source}.0)")
        }
        TypeRef::Named(name)
            if idl
                .enums
                .iter()
                .any(|enumeration| enumeration.name == *name) =>
        {
            encode_enum_value(
                idl,
                idl.enums
                    .iter()
                    .find(|enumeration| enumeration.name == *name)
                    .expect("validated enum exists"),
                source,
            )
        }
        TypeRef::Named(name) => {
            let structure = idl
                .structs
                .iter()
                .find(|structure| structure.name == *name)
                .expect("validated named type exists");
            let fields = structure
                .fields
                .iter()
                .map(|field| encode_value(idl, &field.ty, &format!("{source}.{}", field.name)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("nexa_runtime::HostValue::Struct(vec![{fields}])")
        }
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

fn decode_option(idl: &Idl, inner: &TypeRef, source: &str) -> String {
    let metadata = nexa_bytecode::option_type(value_type(idl, inner));
    let none = &metadata.variants[0];
    let some = &metadata.variants[1];
    let payload = decode_value(idl, inner, "value");
    format!(
        "match {source} {{ nexa_runtime::HostValue::Enum {{ type_id, variant, tag, payload }} \
         if *type_id == nexa_runtime::StableId({type_id}) && *variant == \
         nexa_runtime::StableId({none_id}) && *tag == {none_tag} && payload.is_none() => None, \
         nexa_runtime::HostValue::Enum {{ type_id, variant, tag, payload }} if *type_id == \
         nexa_runtime::StableId({type_id}) && *variant == nexa_runtime::StableId({some_id}) && \
         *tag == {some_tag} => {{ let value = match payload.as_deref() {{ Some(value) => value, \
         None => return Err(nexa_runtime::HostTrap::Type) }}; Some({payload}) }}, _ => return \
         Err(nexa_runtime::HostTrap::Type) }}",
        type_id = metadata.type_id.0,
        none_id = none.stable_id.0,
        none_tag = none.tag,
        some_id = some.stable_id.0,
        some_tag = some.tag,
    )
}

fn decode_result(idl: &Idl, success: &TypeRef, error: &TypeRef, source: &str) -> String {
    let metadata = nexa_bytecode::result_type(value_type(idl, success), value_type(idl, error));
    let ok = &metadata.variants[0];
    let err = &metadata.variants[1];
    let success_payload = decode_value(idl, success, "value");
    let error_payload = decode_value(idl, error, "value");
    format!(
        "match {source} {{ nexa_runtime::HostValue::Enum {{ type_id, variant, tag, payload }} if \
         *type_id == nexa_runtime::StableId({type_id}) && *variant == \
         nexa_runtime::StableId({ok_id}) && *tag == {ok_tag} => {{ let value = match \
         payload.as_deref() {{ Some(value) => value, None => return \
         Err(nexa_runtime::HostTrap::Type) }}; Ok({success_payload}) }}, \
         nexa_runtime::HostValue::Enum {{ type_id, variant, tag, payload }} if *type_id == \
         nexa_runtime::StableId({type_id}) && *variant == nexa_runtime::StableId({err_id}) && \
         *tag == {err_tag} => {{ let value = match payload.as_deref() {{ Some(value) => value, \
         None => return Err(nexa_runtime::HostTrap::Type) }}; Err({error_payload}) }}, _ => return \
         Err(nexa_runtime::HostTrap::Type) }}",
        type_id = metadata.type_id.0,
        ok_id = ok.stable_id.0,
        ok_tag = ok.tag,
        err_id = err.stable_id.0,
        err_tag = err.tag,
    )
}

fn decode_collection(
    idl: &Idl,
    inner: &TypeRef,
    source: &str,
    variant: &str,
    copy_buffer: bool,
) -> String {
    let decoded = decode_value(idl, inner, "value");
    let finish = if copy_buffer {
        "nexa_runtime::CopyBuffer::new(decoded)"
    } else {
        "decoded"
    };
    format!(
        "{{ let values = match {source} {{ nexa_runtime::HostValue::{variant}(values) => values, \
         _ => return Err(nexa_runtime::HostTrap::Type) }}; let mut decoded = \
         Vec::with_capacity(values.len()); for value in values.as_slice() {{ \
         decoded.push({decoded}); }} {finish} }}"
    )
}

fn decode_enum_value(idl: &Idl, enumeration: &Enum, source: &str) -> String {
    let mut output = format!(
        "match {source} {{ nexa_runtime::HostValue::Enum {{ type_id, variant, payload, .. }} \
         if *type_id == nexa_runtime::StableId::from_name(\"{}\") => {{",
        enumeration.name
    );
    for (index, variant) in enumeration.variants.iter().enumerate() {
        if index != 0 {
            output.push_str(" else ");
        }
        write!(
            output,
            "if *variant == nexa_runtime::StableId::from_parts(&[\"{}\", \"::\", \"{}\"]) {{",
            enumeration.name, variant.name
        )
        .expect("String writes do not fail");
        if let Some(payload_type) = &variant.payload {
            let decoded = decode_value(idl, payload_type, "value");
            write!(
                output,
                "let value = match payload.as_deref() {{ Some(value) => value, None => return \
                 Err(nexa_runtime::HostTrap::Type) }}; {}::{}({decoded})",
                enumeration.name, variant.name
            )
            .expect("String writes do not fail");
        } else {
            write!(
                output,
                "if payload.is_some() {{ return Err(nexa_runtime::HostTrap::Type); }} {}::{}",
                enumeration.name, variant.name
            )
            .expect("String writes do not fail");
        }
        output.push('}');
    }
    output.push_str(
        " else { return Err(nexa_runtime::HostTrap::Type) } }, \
         _ => return Err(nexa_runtime::HostTrap::Type) }",
    );
    output
}

fn encode_enum_value(idl: &Idl, enumeration: &Enum, source: &str) -> String {
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
            let payload = encode_value(idl, payload_type, "value");
            write!(
                output,
                "{}::{}(value) => nexa_runtime::HostValue::Enum {{ type_id: {type_id}, \
                 variant: {variant_id}, tag: {tag}, payload: Some(Box::new({payload})) }},",
                enumeration.name, variant.name
            )
            .expect("String writes do not fail");
        } else {
            write!(
                output,
                "{}::{} => nexa_runtime::HostValue::Enum {{ type_id: {type_id}, \
                 variant: {variant_id}, tag: {tag}, payload: None }},",
                enumeration.name, variant.name
            )
            .expect("String writes do not fail");
        }
    }
    output.push('}');
    output
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

fn parameterized_type_name(name: &str, inner: Option<&TypeRef>) -> String {
    inner.map_or_else(
        || name.to_owned(),
        |inner| format!("{name}<{}>", type_name(inner)),
    )
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
        TypeRef::ResourceToken(_) => "nexa_runtime::ResourceTokenHandle".into(),
        TypeRef::Snapshot(_) => "nexa_runtime::SnapshotHandle".into(),
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

    use super::{TypeRef, exact_hash, generate_rust, parse};

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
    fn binding_has_typed_trait_thunk_and_export_marker() {
        let generated = generate_rust(&parse(IDL).unwrap());
        assert!(generated.contains("pub trait GameHost"));
        assert!(generated.contains("entity_position"));
        assert!(generated.contains("THUNK_ENTITY_POSITION"));
        assert!(generated.contains("pub enum Update"));
    }

    #[test]
    fn parses_the_complete_mvr_boundary_type_matrix() {
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
        assert!(struct_payload.contains("HostPayload::Struct"));
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
        fs::write(directory.join("src/bindings.rs"), generate_rust(&idl)).unwrap();
        fs::write(
            directory.join("src/main.rs"),
            r#"mod bindings;
use bindings::{Entity, Event, GameHost, GeneratedHostRegistry, HostError, Vec3};
use nexa_runtime::{HostArgs, HostCallOutcome, HostRegistry, HostValue, RuntimeLimits, RuntimeResources, TaskRuntime};

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

    fn position(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        entity: Entity,
        mut value: Vec3,
    ) -> Result<Vec3, HostError> {
        value.x += entity.0 as f32;
        Ok(value)
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

    fn echo(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        value: String,
    ) -> Result<String, HostError> {
        Ok(value)
    }

    fn echo_event(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        value: Event,
    ) -> Result<Event, HostError> {
        Ok(value)
    }

    fn collections(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        values: Vec<Option<i32>>,
        copy: nexa_runtime::CopyBuffer<i64>,
    ) -> Result<Vec<Result<i32, i32>>, HostError> {
        let offset = copy.as_slice().first().copied().unwrap_or_default() as i32;
        Ok(values
            .into_iter()
            .map(|value| value.map_or(Err(offset), |value| Ok(value + offset)))
            .collect())
    }

    fn explode(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
    ) -> Result<i32, HostError> {
        panic!("host panic")
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
    let values = [HostValue::I32(20), HostValue::I32(22)];
    let mut registry = GeneratedHostRegistry::new(Mock);
    let outcome = registry
        .call(0, &mut context, HostArgs::new(&values))
        .unwrap();
    assert_eq!(outcome, HostCallOutcome::Immediate(HostValue::I32(42)));
    let composite = [
        HostValue::Opaque(2),
        HostValue::Struct(vec![
            HostValue::F32(1.0),
            HostValue::F32(2.0),
            HostValue::F32(3.0),
        ]),
    ];
    assert_eq!(
        registry
            .call(1, &mut context, HostArgs::new(&composite))
            .unwrap(),
        HostCallOutcome::Immediate(HostValue::Struct(vec![
            HostValue::F32(3.0),
            HostValue::F32(2.0),
            HostValue::F32(3.0),
        ]))
    );
    let scalars = [HostValue::I64(4), HostValue::F64(0.5), HostValue::Rune('A')];
    assert_eq!(
        registry.call(2, &mut context, HostArgs::new(&scalars)).unwrap(),
        HostCallOutcome::Immediate(HostValue::F64(69.5))
    );
    assert_eq!(
        registry
            .call(
                3,
                &mut context,
                HostArgs::new(&[HostValue::String("Nexa界".into())]),
            )
            .unwrap(),
        HostCallOutcome::Immediate(HostValue::String("Nexa界".into()))
    );
    let event = HostValue::Enum {
        type_id: nexa_runtime::StableId::from_name("Event"),
        variant: nexa_runtime::StableId::from_parts(&["Event", "::", "Damage"]),
        tag: 1,
        payload: Some(Box::new(HostValue::I32(7))),
    };
    assert_eq!(
        registry
            .call(4, &mut context, HostArgs::new(std::slice::from_ref(&event)))
            .unwrap(),
        HostCallOutcome::Immediate(event)
    );
    assert_eq!(
        registry.call(6, &mut context, HostArgs::new(&[])),
        Err(nexa_runtime::HostTrap::Panicked)
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
