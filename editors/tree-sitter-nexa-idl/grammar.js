const syntax = require("../language-syntax.json");

const nidlKeywords = Object.fromEntries(
  [
    ...syntax.nidl.declarationKeywords,
    ...syntax.nidl.modeKeywords,
    ...syntax.nidl.policyKeywords,
  ].map((keyword) => [keyword, keyword]),
);

module.exports = grammar({
  name: "nexa_idl",

  // NIDL deliberately has no comment tokens. A slash is therefore rejected.
  extras: (_) => [/\s/],

  word: ($) => $.nidl_identifier,

  supertypes: ($) => [$.nidl_member, $.nidl_type],

  rules: {
    source_file: ($) => $.nidl_document,

    nidl_document: ($) =>
      seq(
        field("keyword", $.interface_keyword),
        field("name", $.nidl_type_identifier),
        "{",
        repeat($.nidl_member),
        "}",
      ),

    nidl_member: ($) =>
      choice(
        $.opaque_declaration,
        $.nidl_struct_declaration,
        $.nidl_enum_declaration,
        $.host_function_declaration,
        $.export_declaration,
      ),

    opaque_declaration: ($) =>
      seq(
        field("keyword", $.opaque_keyword),
        field("name", $.nidl_type_identifier),
        ";",
      ),

    nidl_struct_declaration: ($) =>
      seq(
        field("keyword", $.nidl_struct_keyword),
        field("name", $.nidl_type_identifier),
        "{",
        repeat($.nidl_field_declaration),
        "}",
      ),

    nidl_field_declaration: ($) =>
      seq(
        field("name", $.nidl_identifier),
        ":",
        field("type", $.nidl_type),
        ";",
      ),

    nidl_enum_declaration: ($) =>
      seq(
        field("keyword", $.nidl_enum_keyword),
        field("name", $.nidl_type_identifier),
        "{",
        repeat(seq($.nidl_enum_variant, optional(","))),
        "}",
      ),

    nidl_enum_variant: ($) =>
      seq(
        field("name", $.nidl_identifier),
        optional(seq("(", field("payload", $.nidl_type), ")")),
      ),

    host_function_declaration: ($) =>
      seq(
        field("mode", $.host_mode_keyword),
        optional($.request_policy),
        optional($.fuel_clause),
        field("keyword", $.nidl_function_keyword),
        field("name", $.nidl_identifier),
        field("parameters", $.nidl_parameter_list),
        field("arrow", $.return_arrow_operator),
        field("return_type", $.nidl_type),
        ";",
      ),

    request_policy: ($) =>
      seq(
        "(",
        field("cancel", $.cancel_policy_keyword),
        ",",
        field("abandon", $.abandon_policy_keyword),
        ")",
      ),

    fuel_clause: ($) =>
      seq($.fuel_keyword, field("amount", $.integer_literal)),

    nidl_parameter_list: ($) =>
      seq("(", optional(commaSep1($.nidl_parameter)), ")"),

    nidl_parameter: ($) =>
      seq(
        field("name", $.nidl_identifier),
        ":",
        field("type", $.nidl_type),
      ),

    export_declaration: ($) =>
      seq(
        field("keyword", $.export_keyword),
        field("name", $.nidl_identifier),
        field("parameters", $.nidl_parameter_list),
        field("arrow", $.return_arrow_operator),
        field("return_type", choice($.nidl_type, $.void_type)),
        ";",
      ),

    nidl_type: ($) =>
      choice(
        $.nidl_builtin_type,
        $.nidl_generic_type,
        $.nidl_type_identifier,
      ),

    nidl_generic_type: ($) =>
      seq(
        field("name", choice($.nidl_builtin_type, $.nidl_type_identifier)),
        field("arguments", $.nidl_type_argument_list),
      ),

    nidl_type_argument_list: ($) => seq("<", commaSep1($.nidl_type), ">"),
    nidl_type_identifier: ($) => $.nidl_identifier,
    // Keep comment introducers outside the identifier token. Without excluding
    // both characters, text such as `// comment` or `/* comment */` can be
    // accepted as an enum variant even though NIDL is deliberately comment-free.
    nidl_identifier: (_) => /[^/*{}(),:;<>\s-]+/,

    interface_keyword: (_) => nidlKeywords.interface,
    opaque_keyword: (_) => nidlKeywords.opaque,
    nidl_struct_keyword: (_) => nidlKeywords.struct,
    nidl_enum_keyword: (_) => nidlKeywords.enum,
    nidl_function_keyword: (_) => nidlKeywords.fn,
    export_keyword: (_) => nidlKeywords.export,
    host_mode_keyword: (_) =>
      choice(
        ...syntax.nidl.modeKeywords.filter((keyword) => keyword !== "fuel"),
      ),
    fuel_keyword: (_) => nidlKeywords.fuel,
    cancel_policy_keyword: (_) =>
      choice(
        ...syntax.nidl.policyKeywords.filter((keyword) => keyword !== "trap"),
      ),
    abandon_policy_keyword: (_) =>
      choice(
        ...syntax.nidl.policyKeywords.filter(
          (keyword) => keyword !== "cancel_task",
        ),
      ),
    nidl_builtin_type: (_) =>
      choice(
        ...syntax.nidl.builtinTypes.filter((type) => type !== "void"),
      ),
    void_type: (_) =>
      syntax.nidl.builtinTypes.find((type) => type === "void"),

    integer_literal: (_) => /[0-9]+/,
    return_arrow_operator: (_) => "->",
  },
});

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}
