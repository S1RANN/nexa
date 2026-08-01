const syntax = require("../language-syntax.json");

const keywords = Object.fromEntries(
  [
    ...syntax.nidl.declarationKeywords,
    ...syntax.nidl.modeKeywords,
    ...syntax.nidl.attributeKeywords,
    ...syntax.nidl.policyKeywords,
  ].map((keyword) => [keyword, keyword]),
);

module.exports = grammar({
  name: "nexa_idl",

  extras: ($) => [
    /\s/,
    $.doc_comment,
    $.line_comment,
    $.block_comment,
  ],

  word: ($) => $.nidl_identifier,

  supertypes: ($) => [$.contract_member, $.nidl_type],

  rules: {
    source_file: ($) => $.nidl_document,

    doc_comment: (_) => token(prec(2, /\/\/\/[^\n\r]*/)),
    line_comment: (_) => token(prec(1, /\/\/[^\n\r]*/)),
    block_comment: (_) => token(seq("/*", /[^*]*\*+([^/*][^*]*\*+)*/, "/")),

    nidl_document: ($) =>
      seq(
        field("keyword", $.contract_keyword),
        field("name", $.nidl_type_identifier),
        "{",
        repeat($.contract_member),
        "}",
      ),

    contract_member: ($) =>
      choice(
        $.handle_declaration,
        $.nidl_struct_declaration,
        $.nidl_enum_declaration,
        $.host_block,
        $.nexa_block,
      ),

    handle_declaration: ($) =>
      seq(
        field("keyword", $.handle_keyword),
        field("name", $.nidl_type_identifier),
        ";",
      ),

    nidl_struct_declaration: ($) =>
      seq(
        field("keyword", $.nidl_struct_keyword),
        field("name", $.nidl_type_identifier),
        "{",
        optional(commaSep1($.nidl_field_declaration)),
        optional(","),
        "}",
      ),

    nidl_field_declaration: ($) =>
      seq(
        field("name", $.nidl_identifier),
        ":",
        field("type", $.nidl_type),
      ),

    nidl_enum_declaration: ($) =>
      seq(
        field("keyword", $.nidl_enum_keyword),
        field("name", $.nidl_type_identifier),
        "{",
        optional(commaSep1($.nidl_enum_variant)),
        optional(","),
        "}",
      ),

    nidl_enum_variant: ($) =>
      seq(
        field("name", $.nidl_type_identifier),
        optional(
          choice(
            seq(
              "(",
              optional(commaSep1($.nidl_type)),
              optional(","),
              ")",
            ),
            seq(
              "{",
              optional(commaSep1($.nidl_field_declaration)),
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
        repeat($.nidl_attribute),
        optional(field("effect", $.async_keyword)),
        field("keyword", $.nidl_function_keyword),
        field("name", $.nidl_identifier),
        field("parameters", $.nidl_parameter_list),
        optional(
          seq(
            field("arrow", $.return_arrow_operator),
            field("return_type", $.nidl_type),
          ),
        ),
        ";",
      ),

    nexa_function_declaration: ($) =>
      seq(
        repeat($.nidl_attribute),
        optional(field("effect", $.async_keyword)),
        field("keyword", $.nidl_function_keyword),
        field("name", $.nidl_identifier),
        field("parameters", $.nidl_parameter_list),
        optional(
          seq(
            field("arrow", $.return_arrow_operator),
            field("return_type", $.nidl_type),
          ),
        ),
        ";",
      ),

    nidl_attribute: ($) =>
      seq(
        "@",
        field("name", $.nidl_attribute_name),
        optional(
          seq(
            "(",
            optional(commaSep1($.nidl_attribute_argument)),
            optional(","),
            ")",
          ),
        ),
      ),

    nidl_attribute_argument: ($) =>
      choice(
        seq(
          field("name", $.nidl_identifier),
          "=",
          field("value", $.nidl_attribute_value),
        ),
        field("value", $.nidl_attribute_value),
      ),

    nidl_attribute_value: ($) =>
      choice(
        $.string_literal,
        $.integer_literal,
        $.policy_value,
        $.nidl_identifier,
      ),

    nidl_parameter_list: ($) =>
      seq("(", optional(commaSep1($.nidl_parameter)), optional(","), ")"),

    nidl_parameter: ($) =>
      seq(
        field("name", $.nidl_identifier),
        ":",
        field("type", $.nidl_type),
      ),

    nidl_type: ($) =>
      choice(
        $.nidl_generic_type,
        $.nidl_builtin_type,
        $.nidl_type_identifier,
      ),

    nidl_generic_type: ($) =>
      seq(
        field("name", choice($.nidl_builtin_type, $.nidl_type_identifier)),
        field("arguments", $.nidl_type_argument_list),
      ),

    nidl_type_argument_list: ($) =>
      seq("<", commaSep1($.nidl_type), optional(","), ">"),
    nidl_type_identifier: ($) => $.nidl_identifier,
    nidl_identifier: (_) => /[A-Za-z_][A-Za-z0-9_]*/,

    contract_keyword: (_) => keywords.contract,
    handle_keyword: (_) => keywords.handle,
    nidl_struct_keyword: (_) => keywords.struct,
    nidl_enum_keyword: (_) => keywords.enum,
    host_keyword: (_) => keywords.host,
    nexa_keyword: (_) => keywords.nexa,
    async_keyword: (_) => keywords.async,
    nidl_function_keyword: (_) => keywords.fn,
    nidl_attribute_name: ($) =>
      choice(...syntax.nidl.attributeKeywords, $.nidl_identifier),
    policy_value: (_) => choice(...syntax.nidl.policyKeywords),
    nidl_builtin_type: (_) => choice(...syntax.nidl.builtinTypes),

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
