const syntax = require("../language-syntax.json");

const keywords = Object.fromEntries(
  [
    ...syntax.contract.declarationKeywords,
    ...syntax.contract.modeKeywords,
    ...syntax.contract.attributeKeywords,
    ...syntax.contract.policyKeywords,
  ].map((keyword) => [keyword, keyword]),
);

module.exports = grammar({
  name: "nexa_contract",

  extras: ($) => [
    /\s/,
    $.doc_comment,
    $.line_comment,
    $.block_comment,
  ],

  word: ($) => $.contract_identifier,

  supertypes: ($) => [$.contract_member, $.contract_type],

  rules: {
    source_file: ($) => $.contract_document,

    doc_comment: (_) => token(prec(2, /\/\/\/[^\n\r]*/)),
    line_comment: (_) => token(prec(1, /\/\/[^\n\r]*/)),
    block_comment: (_) => token(seq("/*", /[^*]*\*+([^/*][^*]*\*+)*/, "/")),

    contract_document: ($) =>
      seq(
        field("header", $.contract_header),
        repeat($.contract_member),
      ),

    // Contract Syntax v3 (flat): `@attr... contract <Name>;` is the file-level header, then
    // struct/enum/handle/host/nexa items follow at the top level with no outer block. Header
    // attributes (e.g. `@stable("...")`) and doc comments attach to this header.
    contract_header: ($) =>
      seq(
        repeat($.contract_attribute),
        field("keyword", $.contract_keyword),
        field("name", $.contract_type_identifier),
        ";",
      ),

    contract_member: ($) =>
      choice(
        $.handle_declaration,
        $.contract_struct_declaration,
        $.contract_enum_declaration,
        $.host_block,
        $.nexa_block,
      ),

    handle_declaration: ($) =>
      seq(
        field("keyword", $.handle_keyword),
        field("name", $.contract_type_identifier),
        ";",
      ),

    contract_struct_declaration: ($) =>
      seq(
        field("keyword", $.contract_struct_keyword),
        field("name", $.contract_type_identifier),
        "{",
        optional(commaSep1($.contract_field_declaration)),
        optional(","),
        "}",
      ),

    contract_field_declaration: ($) =>
      seq(
        field("name", $.contract_identifier),
        ":",
        field("type", $.contract_type),
      ),

    contract_enum_declaration: ($) =>
      seq(
        field("keyword", $.contract_enum_keyword),
        field("name", $.contract_type_identifier),
        "{",
        optional(commaSep1($.contract_enum_variant)),
        optional(","),
        "}",
      ),

    contract_enum_variant: ($) =>
      seq(
        field("name", $.contract_type_identifier),
        optional(
          choice(
            seq(
              "(",
              optional(commaSep1($.contract_type)),
              optional(","),
              ")",
            ),
            seq(
              "{",
              optional(commaSep1($.contract_field_declaration)),
              optional(","),
              "}",
            ),
          ),
        ),
      ),

    host_block: ($) =>
      seq(
        field("keyword", $.host_keyword),
        "{",
        repeat($.host_function_declaration),
        "}",
      ),

    nexa_block: ($) =>
      seq(
        field("keyword", $.nexa_keyword),
        "{",
        repeat($.nexa_function_declaration),
        "}",
      ),

    host_function_declaration: ($) =>
      seq(
        repeat($.contract_attribute),
        optional(field("effect", $.async_keyword)),
        field("keyword", $.contract_function_keyword),
        field("name", $.contract_identifier),
        field("parameters", $.contract_parameter_list),
        optional(
          seq(
            field("arrow", $.return_arrow_operator),
            field("return_type", $.contract_type),
          ),
        ),
        ";",
      ),

    nexa_function_declaration: ($) =>
      seq(
        repeat($.contract_attribute),
        optional(field("effect", $.async_keyword)),
        field("keyword", $.contract_function_keyword),
        field("name", $.contract_identifier),
        field("parameters", $.contract_parameter_list),
        optional(
          seq(
            field("arrow", $.return_arrow_operator),
            field("return_type", $.contract_type),
          ),
        ),
        ";",
      ),

    contract_attribute: ($) =>
      seq(
        "@",
        field("name", $.contract_attribute_name),
        optional(
          seq(
            "(",
            optional(commaSep1($.contract_attribute_argument)),
            optional(","),
            ")",
          ),
        ),
      ),

    contract_attribute_argument: ($) =>
      choice(
        seq(
          field("name", $.contract_identifier),
          "=",
          field("value", $.contract_attribute_value),
        ),
        field("value", $.contract_attribute_value),
      ),

    contract_attribute_value: ($) =>
      choice(
        $.string_literal,
        $.integer_literal,
        $.policy_value,
        $.contract_identifier,
      ),

    contract_parameter_list: ($) =>
      seq("(", optional(commaSep1($.contract_parameter)), optional(","), ")"),

    contract_parameter: ($) =>
      seq(
        field("name", $.contract_identifier),
        ":",
        field("type", $.contract_type),
      ),

    contract_type: ($) =>
      choice(
        $.contract_generic_type,
        $.contract_builtin_type,
        $.contract_type_identifier,
      ),

    contract_generic_type: ($) =>
      seq(
        field("name", choice($.contract_builtin_type, $.contract_type_identifier)),
        field("arguments", $.contract_type_argument_list),
      ),

    contract_type_argument_list: ($) =>
      seq("<", commaSep1($.contract_type), optional(","), ">"),
    contract_type_identifier: ($) => $.contract_identifier,
    contract_identifier: (_) => /[A-Za-z_][A-Za-z0-9_]*/,

    contract_keyword: (_) => keywords.contract,
    handle_keyword: (_) => keywords.handle,
    contract_struct_keyword: (_) => keywords.struct,
    contract_enum_keyword: (_) => keywords.enum,
    host_keyword: (_) => keywords.host,
    nexa_keyword: (_) => keywords.nexa,
    async_keyword: (_) => keywords.async,
    contract_function_keyword: (_) => keywords.fn,
    contract_attribute_name: ($) =>
      choice(...syntax.contract.attributeKeywords, $.contract_identifier),
    policy_value: (_) => choice(...syntax.contract.policyKeywords),
    contract_builtin_type: (_) => choice(...syntax.contract.builtinTypes),

    string_literal: ($) =>
      seq('"', repeat(choice($.string_content, $.escape_sequence)), '"'),
    string_content: (_) => token.immediate(prec(1, /[^"\\]+/)),
    escape_sequence: (_) => token.immediate(/\\[nrt\\"']/),
    integer_literal: (_) => /[0-9]+/,
    return_arrow_operator: (_) => "->",
  },
});

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}
