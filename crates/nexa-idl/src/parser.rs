use nexa_core::{FileId, SourceSpan};
use nexa_syntax::{
    ContractAst as SyntaxAst, ContractAttribute as SyntaxAttribute,
    ContractAttributeArgument as SyntaxAttributeArgument, ContractAttributeValue as SyntaxAttributeValue,
    ContractItem, ContractDocComment as SyntaxDocComment, ContractEnumDecl as SyntaxEnum,
    ContractField as SyntaxField, ContractFunction as SyntaxFunction,
    ContractFunctionBlock as SyntaxFunctionBlock, ContractFunctionBlockKind, ContractHandleDecl as SyntaxHandle,
    ContractParameter as SyntaxParameter, ContractStructDecl as SyntaxStruct, ContractTypeRef as SyntaxTypeRef,
    ContractVariant as SyntaxVariant, TextRange, parse_contract, parse_contract_ast,
};

use crate::model::{
    Attribute, AttributeArgument, AttributeValue, ContractDecl, DocComment, EnumDecl, FieldDecl,
    FunctionBlock, FunctionDecl, HandleDecl, NidlAst, NidlError, NidlErrorKind, ParameterDecl,
    StructDecl, TypeKind, TypeRef, VariantDecl,
};

pub fn parse(source: &str) -> Result<NidlAst, Vec<NidlError>> {
    parse_with_file_id(source, FileId(0))
}

pub fn parse_with_file_id(source: &str, file: FileId) -> Result<NidlAst, Vec<NidlError>> {
    let tree = parse_contract(source).map_err(|error| {
        vec![NidlError::syntax(
            SourceSpan::new(file, 0, 0),
            format!(
                "NIDL source is too large for 32-bit source spans ({} bytes)",
                error.bytes
            ),
        )]
    })?;
    let syntax = parse_contract_ast(&tree).map_err(|errors| {
        errors
            .into_iter()
            .map(|error| NidlError::syntax(span(file, error.range), error.message))
            .collect::<Vec<_>>()
    })?;
    lower_ast(&syntax, file).map_err(|error| vec![error])
}

fn lower_ast(ast: &SyntaxAst, file: FileId) -> Result<NidlAst, NidlError> {
    let contract = &ast.contract;
    let mut handles = Vec::new();
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    let mut host = None;
    let mut nexa = None;

    for item in &contract.items {
        match item {
            ContractItem::Handle(handle) => handles.push(lower_handle(handle, file)?),
            ContractItem::Struct(structure) => {
                structs.push(lower_struct(structure, file)?);
            }
            ContractItem::Enum(enumeration) => {
                enums.push(lower_enum(enumeration, file)?);
            }
            ContractItem::FunctionBlock(block) => {
                let lowered = lower_function_block(block, file)?;
                let target = match block.kind {
                    ContractFunctionBlockKind::Host => &mut host,
                    ContractFunctionBlockKind::Nexa => &mut nexa,
                };
                if target.replace(lowered).is_some() {
                    let name = match block.kind {
                        ContractFunctionBlockKind::Host => "host",
                        ContractFunctionBlockKind::Nexa => "nexa",
                    };
                    return Err(NidlError::new(
                        NidlErrorKind::Duplicate,
                        span(file, block.range),
                        format!("a contract may contain at most one `{name}` block"),
                    ));
                }
            }
        }
    }

    let contract_span = span(file, contract.range);
    Ok(NidlAst {
        source: ast.source.as_str().to_owned(),
        span: span(file, ast.range),
        contract: ContractDecl {
            name: contract.name.text.clone(),
            name_span: span(file, contract.name.range),
            span: contract_span,
            docs: lower_docs(&contract.docs, file),
            attributes: lower_attributes(&contract.attributes, file)?,
            handles,
            structs,
            enums,
            host,
            nexa,
        },
    })
}

fn lower_handle(handle: &SyntaxHandle, file: FileId) -> Result<HandleDecl, NidlError> {
    Ok(HandleDecl {
        name: handle.name.text.clone(),
        name_span: span(file, handle.name.range),
        span: span(file, handle.range),
        docs: lower_docs(&handle.docs, file),
        attributes: lower_attributes(&handle.attributes, file)?,
    })
}

fn lower_struct(structure: &SyntaxStruct, file: FileId) -> Result<StructDecl, NidlError> {
    Ok(StructDecl {
        name: structure.name.text.clone(),
        name_span: span(file, structure.name.range),
        span: span(file, structure.range),
        docs: lower_docs(&structure.docs, file),
        attributes: lower_attributes(&structure.attributes, file)?,
        fields: structure
            .fields
            .iter()
            .map(|field| lower_field(field, file))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn lower_field(field: &SyntaxField, file: FileId) -> Result<FieldDecl, NidlError> {
    Ok(FieldDecl {
        name: field.name.text.clone(),
        name_span: span(file, field.name.range),
        span: span(file, field.range),
        docs: lower_docs(&field.docs, file),
        attributes: lower_attributes(&field.attributes, file)?,
        ty: lower_type(&field.ty, file)?,
    })
}

fn lower_enum(enumeration: &SyntaxEnum, file: FileId) -> Result<EnumDecl, NidlError> {
    Ok(EnumDecl {
        name: enumeration.name.text.clone(),
        name_span: span(file, enumeration.name.range),
        span: span(file, enumeration.range),
        docs: lower_docs(&enumeration.docs, file),
        attributes: lower_attributes(&enumeration.attributes, file)?,
        variants: enumeration
            .variants
            .iter()
            .map(|variant| lower_variant(variant, file))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn lower_variant(variant: &SyntaxVariant, file: FileId) -> Result<VariantDecl, NidlError> {
    Ok(VariantDecl {
        name: variant.name.text.clone(),
        name_span: span(file, variant.name.range),
        span: span(file, variant.range),
        docs: lower_docs(&variant.docs, file),
        attributes: lower_attributes(&variant.attributes, file)?,
        payload: variant
            .payload
            .as_ref()
            .map(|payload| lower_type(payload, file))
            .transpose()?,
    })
}

fn lower_function_block(
    block: &SyntaxFunctionBlock,
    file: FileId,
) -> Result<FunctionBlock, NidlError> {
    Ok(FunctionBlock {
        span: span(file, block.range),
        docs: lower_docs(&block.docs, file),
        attributes: lower_attributes(&block.attributes, file)?,
        functions: block
            .functions
            .iter()
            .map(|function| lower_function(function, file))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn lower_function(function: &SyntaxFunction, file: FileId) -> Result<FunctionDecl, NidlError> {
    Ok(FunctionDecl {
        name: function.name.text.clone(),
        name_span: span(file, function.name.range),
        span: span(file, function.range),
        docs: lower_docs(&function.docs, file),
        attributes: lower_attributes(&function.attributes, file)?,
        is_async: function.is_async,
        parameters: function
            .parameters
            .iter()
            .map(|parameter| lower_parameter(parameter, file))
            .collect::<Result<Vec<_>, _>>()?,
        result: function
            .result
            .as_ref()
            .map(|result| lower_type(result, file))
            .transpose()?,
    })
}

fn lower_parameter(parameter: &SyntaxParameter, file: FileId) -> Result<ParameterDecl, NidlError> {
    Ok(ParameterDecl {
        name: parameter.name.text.clone(),
        name_span: span(file, parameter.name.range),
        span: span(file, parameter.range),
        docs: lower_docs(&parameter.docs, file),
        attributes: lower_attributes(&parameter.attributes, file)?,
        ty: lower_type(&parameter.ty, file)?,
    })
}

fn lower_type(ty: &SyntaxTypeRef, file: FileId) -> Result<TypeRef, NidlError> {
    let lowered_arguments = ty
        .arguments
        .iter()
        .map(|argument| lower_type(argument, file))
        .collect::<Result<Vec<_>, _>>()?;
    let kind = match (ty.name.text.as_str(), lowered_arguments.as_slice()) {
        ("i32", []) => TypeKind::I32,
        ("i64", []) => TypeKind::I64,
        ("f32", []) => TypeKind::F32,
        ("f64", []) => TypeKind::F64,
        ("bool", []) => TypeKind::Bool,
        ("rune", []) => TypeKind::Rune,
        ("string", []) => TypeKind::String,
        ("Array", [inner]) => TypeKind::Array(Box::new(inner.clone())),
        ("Buffer", [inner]) => TypeKind::Buffer(Box::new(inner.clone())),
        ("Option", [inner]) => TypeKind::Option(Box::new(inner.clone())),
        ("Result", [ok, error]) => TypeKind::Result(Box::new(ok.clone()), Box::new(error.clone())),
        ("Token", [inner]) => TypeKind::Token(Box::new(inner.clone())),
        ("Snapshot", [inner]) => TypeKind::Snapshot(Box::new(inner.clone())),
        (
            "i32" | "i64" | "f32" | "f64" | "bool" | "rune" | "string" | "Array" | "Buffer"
            | "Option" | "Result" | "Token" | "Snapshot",
            _,
        ) => {
            return Err(NidlError::new(
                NidlErrorKind::UnknownType,
                span(file, ty.range),
                format!(
                    "NIDL type `{}` has {} type argument(s), which is not valid in NIDL v2",
                    ty.name.text,
                    lowered_arguments.len()
                ),
            ));
        }
        (name, []) => TypeKind::Named(name.to_owned()),
        (name, _) => {
            return Err(NidlError::new(
                NidlErrorKind::UnknownType,
                span(file, ty.range),
                format!("named NIDL type `{name}` cannot have type arguments"),
            ));
        }
    };
    Ok(TypeRef {
        kind,
        span: span(file, ty.range),
    })
}

fn lower_docs(docs: &[SyntaxDocComment], file: FileId) -> Vec<DocComment> {
    docs.iter()
        .map(|doc| DocComment {
            text: doc.text.strip_prefix("///").unwrap_or(&doc.text).to_owned(),
            span: span(file, doc.range),
        })
        .collect()
}

fn lower_attributes(
    attributes: &[SyntaxAttribute],
    file: FileId,
) -> Result<Vec<Attribute>, NidlError> {
    attributes
        .iter()
        .map(|attribute| lower_attribute(attribute, file))
        .collect()
}

fn lower_attribute(attribute: &SyntaxAttribute, file: FileId) -> Result<Attribute, NidlError> {
    Ok(Attribute {
        name: attribute.name.text.clone(),
        name_span: span(file, attribute.name.range),
        span: span(file, attribute.range),
        arguments: attribute
            .arguments
            .iter()
            .map(|argument| lower_attribute_argument(argument, file))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn lower_attribute_argument(
    argument: &SyntaxAttributeArgument,
    file: FileId,
) -> Result<AttributeArgument, NidlError> {
    let value = match &argument.value {
        SyntaxAttributeValue::Identifier(identifier) => {
            AttributeValue::Identifier(identifier.text.clone())
        }
        SyntaxAttributeValue::String { cooked, .. } => AttributeValue::String(cooked.clone()),
        SyntaxAttributeValue::Integer { raw, range } => {
            AttributeValue::Integer(raw.parse().map_err(|_| {
                NidlError::new(
                    NidlErrorKind::InvalidAttribute,
                    span(file, *range),
                    format!("integer attribute argument `{raw}` is out of range"),
                )
            })?)
        }
    };
    Ok(AttributeArgument {
        name: argument.name.as_ref().map(|name| name.text.clone()),
        value,
        span: span(file, argument.range),
    })
}

const fn span(file: FileId, range: TextRange) -> SourceSpan {
    SourceSpan::new(file, range.start.get(), range.end.get())
}
