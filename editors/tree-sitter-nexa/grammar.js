const syntax = require("../language-syntax.json");

const keywords = Object.fromEntries(
  [
    ...syntax.nexa.declarationKeywords,
    ...syntax.nexa.visibilityKeywords,
    ...syntax.nexa.effectKeywords,
    ...syntax.nexa.statementKeywords,
    ...syntax.nexa.attributeKeywords,
    ...syntax.nexa.literalKeywords,
  ].map((keyword) => [keyword, keyword]),
);

const PREC = {
  LOGICAL_OR: 1,
  LOGICAL_AND: 2,
  EQUALITY: 3,
  COMPARISON: 4,
  ADDITIVE: 5,
  MULTIPLICATIVE: 6,
  UNARY: 7,
  POSTFIX: 8,
};

module.exports = grammar({
  name: "nexa",

  extras: ($) => [
    /[\s\uFEFF\u2060\u200B]/,
    $.doc_comment,
    $.line_comment,
    $.block_comment,
  ],

  word: ($) => $.lower_identifier,

  supertypes: ($) => [
    $.declaration,
    $.statement,
    $.expression,
    $.primary_expression,
    $.type,
  ],

  conflicts: ($) => [
    [$.expression_path_segment, $.type_path_segment],
    [$.primary_expression, $.generic_call_expression],
  ],

  rules: {
    source_file: ($) =>
      repeat(choice($.use_declaration, $.declaration, $.statement)),

    doc_comment: (_) => token(prec(2, /\/\/\/[^\n\r]*/)),
    line_comment: (_) => token(prec(1, /\/\/[^\n\r]*/)),
    block_comment: (_) => token(seq("/*", /[^*]*\*+([^/*][^*]*\*+)*/, "/")),

    use_declaration: ($) =>
      seq(
        field("keyword", $.use_keyword),
        field("path", $.namespace_path),
        optional(
          seq(
            field("alias_keyword", $.as_keyword),
            field("alias", $.lower_identifier),
          ),
        ),
        ";",
      ),

    namespace_path: ($) =>
      seq(
        field("root", $.identifier),
        repeat1(seq("::", field("segment", $.identifier))),
      ),

    declaration: ($) =>
      choice(
        $.struct_declaration,
        $.enum_declaration,
        $.class_declaration,
        $.function_declaration,
        $.const_declaration,
      ),

    attribute: ($) =>
      seq(
        "@",
        field("name", $.attribute_name),
        optional(
          seq(
            "(",
            optional(commaSep1($.attribute_argument)),
            optional(","),
            ")",
          ),
        ),
      ),

    attribute_argument: ($) =>
      choice(
        seq(
          field("name", $.identifier),
          "=",
          field("value", $.attribute_value),
        ),
        field("value", $.attribute_value),
      ),

    attribute_value: ($) =>
      choice(
        $.string_literal,
        $.integer_literal,
        $.boolean_literal,
        $.identifier,
      ),

    visibility: ($) =>
      choice(
        $.pub_keyword,
        $.package_keyword,
        seq($.pub_keyword, "(", $.package_keyword, ")"),
      ),

    struct_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        field("keyword", $.struct_keyword),
        field("name", $.type_identifier),
        optional(field("type_parameters", $.generic_parameter_list)),
        optional(field("where_clause", $.where_clause)),
        field("body", $.field_declaration_block),
      ),

    class_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        field("keyword", $.class_keyword),
        field("name", $.type_identifier),
        optional(field("type_parameters", $.generic_parameter_list)),
        optional(field("where_clause", $.where_clause)),
        field("body", $.field_declaration_block),
      ),

    enum_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        field("keyword", $.enum_keyword),
        field("name", $.type_identifier),
        optional(field("type_parameters", $.generic_parameter_list)),
        optional(field("where_clause", $.where_clause)),
        field("body", $.enum_variant_block),
      ),

    const_declaration: ($) =>
      prec(
        1,
        seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        field("keyword", $.const_keyword),
        field("name", $.identifier),
        ":",
        field("type", $.type),
        "=",
        field("value", $.expression),
        ";",
        ),
      ),

    field_declaration_block: ($) =>
      seq("{", optional(commaSep1($.field_declaration)), optional(","), "}"),

    field_declaration: ($) =>
      seq(
        repeat($.attribute),
        field("name", $.identifier),
        ":",
        field("type", $.type),
      ),

    enum_variant_block: ($) =>
      seq("{", optional(commaSep1($.enum_variant)), optional(","), "}"),

    enum_variant: ($) =>
      seq(
        field("name", $.type_identifier),
        optional(
          choice(
            seq("(", optional(commaSep1($.type)), optional(","), ")"),
            field("fields", $.field_declaration_block),
          ),
        ),
      ),

    function_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        optional(field("effect", $.async_keyword)),
        field("keyword", $.function_keyword),
        field("name", $.identifier),
        optional(field("type_parameters", $.generic_parameter_list)),
        field("parameters", $.parameter_list),
        optional(
          seq(
            field("arrow", $.return_arrow_operator),
            field("return_type", $.type),
          ),
        ),
        optional(field("where_clause", $.where_clause)),
        field("body", $.block),
      ),

    generic_parameter_list: ($) =>
      seq("<", commaSep1($.generic_parameter), optional(","), ">"),

    generic_parameter: ($) =>
      seq(
        field("name", $.type_identifier),
        optional(seq(":", field("bounds", $.generic_bound_list))),
      ),

    generic_bound_list: ($) =>
      seq($.generic_bound, repeat(seq("+", $.generic_bound))),

    generic_bound: ($) =>
      seq(
        field("name", $.type_path),
        optional(field("bindings", $.associated_output_binding_list)),
      ),

    associated_output_binding_list: ($) =>
      seq("<", commaSep1($.associated_output_binding), optional(","), ">"),

    associated_output_binding: ($) =>
      seq(field("name", $.identifier), "=", field("type", $.type)),

    where_clause: ($) =>
      seq(
        field("keyword", $.where_keyword),
        commaSep1($.where_predicate),
        optional(","),
      ),

    where_predicate: ($) =>
      seq(
        field("type", $.type),
        ":",
        field("bounds", $.generic_bound_list),
      ),

    parameter_list: ($) =>
      seq("(", optional(commaSep1($.parameter)), optional(","), ")"),

    parameter: ($) =>
      seq(
        field("name", $.identifier),
        ":",
        field("type", $.type),
      ),

    block: ($) => seq("{", repeat($.statement), "}"),

    statement: ($) =>
      choice(
        $.return_statement,
        $.binding_statement,
        $.if_statement,
        $.for_statement,
        $.while_statement,
        $.break_statement,
        $.continue_statement,
        $.yield_statement,
        $.defer_statement,
        $.assignment_statement,
        $.expression_statement,
      ),

    return_statement: ($) =>
      seq($.return_keyword, optional(field("value", $.expression)), ";"),

    binding_statement: ($) =>
      seq(
        field("keyword", choice($.let_keyword, $.const_keyword)),
        field("name", $.identifier),
        optional(seq(":", field("type", $.type))),
        "=",
        field("value", $.expression),
        ";",
      ),

    if_statement: ($) =>
      prec.right(
        seq(
          $.if_keyword,
          field("condition", $.expression),
          field("consequence", $.block),
          optional(
            seq(
              $.else_keyword,
              field("alternative", choice($.block, $.if_statement)),
            ),
          ),
        ),
      ),

    for_statement: ($) =>
      choice(
        seq(
          $.for_keyword,
          field("variable", $.identifier),
          $.in_keyword,
          field("start", $.expression),
          "..",
          field("end", $.expression),
          field("body", $.block),
        ),
        seq(
          $.for_keyword,
          field("variable", choice($.identifier, $.pair_binding)),
          $.in_keyword,
          field("iterable", $.expression),
          field("body", $.block),
        ),
      ),

    pair_binding: ($) =>
      seq(
        "(",
        field("key", $.identifier),
        ",",
        field("value", $.identifier),
        ")"
      ),

    while_statement: ($) =>
      seq(
        $.while_keyword,
        field("condition", $.expression),
        field("body", $.block),
      ),

    break_statement: ($) => seq($.break_keyword, ";"),
    continue_statement: ($) => seq($.continue_keyword, ";"),
    yield_statement: ($) => seq($.yield_keyword, ";"),

    defer_statement: ($) =>
      seq($.defer_keyword, field("value", choice($.block, $.expression)), ";"),

    assignment_statement: ($) =>
      seq(
        field("target", choice($.path_expression, $.postfix_expression)),
        field(
          "operator",
          alias(choice("=", "+=", "-=", "*=", "/=", "%="), $.operator),
        ),
        field("value", $.expression),
        ";",
      ),

    expression_statement: ($) => seq($.expression, ";"),

    expression: ($) =>
      choice(
        $.match_expression,
        $.binary_expression,
        $.unary_expression,
        $.postfix_expression,
        $.primary_expression,
      ),

    match_expression: ($) =>
      prec.right(
        seq(
          $.match_keyword,
          field("value", $.expression),
          "{",
          optional(commaSep1($.match_arm)),
          optional(","),
          "}",
        ),
      ),

    match_arm: ($) =>
      seq(
        field("pattern", $.match_pattern),
        field("arrow", $.match_arrow_operator),
        field("value", choice($.block, $.expression)),
      ),

    match_pattern: ($) =>
      seq(
        $.path_expression,
        optional(
          choice(
            seq(
              "(",
              optional(commaSep1($.match_pattern_binding)),
              optional(","),
              ")",
            ),
            seq(
              "{",
              optional(commaSep1($.match_pattern_binding)),
              optional(","),
              "}",
            ),
          ),
        ),
      ),

    match_pattern_binding: ($) =>
      choice(
        $.identifier,
        seq(field("field", $.identifier), ":", field("binding", $.identifier)),
      ),

    unary_expression: ($) =>
      prec(
        PREC.UNARY,
        seq(field("operator", alias(choice("!", "-"), $.operator)), $.expression),
      ),

    binary_expression: ($) =>
      choice(
        binary($, PREC.LOGICAL_OR, "||"),
        binary($, PREC.LOGICAL_AND, "&&"),
        binary($, PREC.EQUALITY, choice("==", "!=")),
        binary($, PREC.COMPARISON, choice("<", "<=", ">", ">=")),
        binary($, PREC.ADDITIVE, choice("+", "-")),
        binary($, PREC.MULTIPLICATIVE, choice("*", "/", "%")),
      ),

    postfix_expression: ($) =>
      prec.left(
        PREC.POSTFIX,
        seq(
          field("operand", choice($.primary_expression, $.postfix_expression)),
          field(
            "operation",
            choice(
              $.call_suffix,
              $.field_suffix,
              $.await_suffix,
              $.try_suffix,
              $.index_suffix,
            ),
          ),
        ),
      ),

    call_suffix: ($) =>
      field("arguments", $.argument_list),

    field_suffix: ($) => seq(".", field("property", $.identifier)),
    await_suffix: ($) => seq(".", field("keyword", $.await_keyword)),
    try_suffix: (_) => "?",
    index_suffix: ($) => seq("[", field("index", $.expression), "]"),

    argument_list: ($) =>
      seq("(", optional(commaSep1($.expression)), optional(","), ")"),

    primary_expression: ($) =>
      choice(
        $.generic_call_expression,
        $.aggregate_literal,
        $.array_literal,
        $.tuple_expression,
        $.parenthesized_expression,
        $.path_expression,
        $.boolean_literal,
        $.float_literal,
        $.integer_literal,
        $.rune_literal,
        $.string_literal,
      ),

    generic_call_expression: ($) =>
      prec.dynamic(
        1,
        seq(
          field("callee", $.path_expression),
          field("type_arguments", $.type_argument_list),
          field("arguments", $.argument_list),
        ),
      ),

    array_literal: ($) =>
      seq("[", optional(commaSep1($.expression)), optional(","), "]"),

    aggregate_literal: ($) =>
      prec(
        PREC.POSTFIX,
        seq(
          field("type", $.type_path),
          field("fields", $.field_initializer_block),
        ),
      ),

    field_initializer_block: ($) =>
      seq("{", optional(commaSep1($.field_initializer)), optional(","), "}"),

    field_initializer: ($) =>
      choice(
        seq(
          field("name", $.identifier),
          ":",
          field("value", $.expression),
        ),
        field("shorthand", $.identifier),
        seq("..", field("base", $.expression)),
      ),

    parenthesized_expression: ($) => seq("(", $.expression, ")"),

    tuple_expression: ($) =>
      seq(
        "(",
        field("element", $.expression),
        ",",
        optional(commaSep1(field("element", $.expression))),
        ")",
      ),

    path_expression: ($) =>
      seq(
        $.expression_path_segment,
        repeat(seq("::", $.expression_path_segment)),
      ),

    expression_path_segment: ($) => choice($.identifier, $.builtin_type),

    type: ($) => choice($.generic_type, $.tuple_type, $.type_path),

    tuple_type: ($) =>
      seq(
        "(",
        field("element", $.type),
        ",",
        optional(commaSep1(field("element", $.type))),
        ")",
      ),

    generic_type: ($) =>
      seq(
        field("name", $.type_path),
        field("arguments", $.type_argument_list),
      ),

    type_argument_list: ($) =>
      seq("<", commaSep1($.type_argument), optional(","), ">"),

    type_argument: ($) =>
      choice(
        $.generic_type_argument,
        $.tuple_type_argument,
        $.type_argument_path,
      ),

    generic_type_argument: ($) =>
      seq(
        field("name", $.type_argument_path),
        field("arguments", $.type_argument_list),
      ),

    tuple_type_argument: ($) =>
      seq(
        "(",
        field("element", $.type_argument),
        ",",
        optional(commaSep1(field("element", $.type_argument))),
        ")",
      ),

    type_argument_path: ($) =>
      choice(
        $.builtin_type,
        $.upper_identifier,
        seq(
          $.identifier,
          repeat(seq("::", $.identifier)),
          "::",
          choice($.builtin_type, $.upper_identifier),
        ),
      ),

    type_path: ($) =>
      seq($.type_path_segment, repeat(seq("::", $.type_path_segment))),

    type_path_segment: ($) => choice($.builtin_type, $.identifier),
    type_identifier: ($) => $.upper_identifier,
    identifier: ($) => choice($.lower_identifier, $.upper_identifier),
    lower_identifier: (_) => /[a-z_][A-Za-z0-9_]*/,
    upper_identifier: (_) => /[A-Z][A-Za-z0-9_]*/,
    builtin_type: (_) => choice(...syntax.nexa.builtinTypes),
    boolean_literal: (_) => choice(...syntax.nexa.literalKeywords),
    integer_literal: (_) => /[0-9]+/,
    float_literal: (_) => /[0-9]+\.[0-9]+/,

    string_literal: ($) =>
      seq(
        '"',
        repeat(
          choice(
            $.string_content,
            $.escape_sequence,
            $.interpolation,
            alias("$", $.string_content),
          ),
        ),
        '"',
      ),

    string_content: (_) => token.immediate(prec(1, /[^"\\$]+/)),
    escape_sequence: (_) => token.immediate(/\\(?:[nrt\\"']|\$\{)/),
    interpolation: ($) =>
      seq(token.immediate("${"), field("expression", $.expression), "}"),

    rune_literal: (_) =>
      token(seq("'", choice(/[^'\\]/, /\\[nrt\\']/), "'")),

    attribute_name: ($) =>
      choice(...syntax.nexa.attributeKeywords, $.identifier),
    use_keyword: (_) => keywords.use,
    as_keyword: (_) => keywords.as,
    function_keyword: (_) => keywords.fn,
    struct_keyword: (_) => keywords.struct,
    enum_keyword: (_) => keywords.enum,
    class_keyword: (_) => keywords.class,
    const_keyword: (_) => keywords.const,
    where_keyword: (_) => keywords.where,
    pub_keyword: (_) => keywords.pub,
    package_keyword: (_) => keywords.package,
    async_keyword: (_) => keywords.async,
    return_keyword: (_) => keywords.return,
    let_keyword: (_) => keywords.let,
    if_keyword: (_) => keywords.if,
    else_keyword: (_) => keywords.else,
    while_keyword: (_) => keywords.while,
    match_keyword: (_) => keywords.match,
    await_keyword: (_) => keywords.await,
    yield_keyword: (_) => keywords.yield,
    defer_keyword: (_) => keywords.defer,
    for_keyword: (_) => keywords.for,
    in_keyword: (_) => keywords.in,
    break_keyword: (_) => keywords.break,
    continue_keyword: (_) => keywords.continue,

    return_arrow_operator: (_) => "->",
    match_arrow_operator: (_) => "=>",
    operator: (_) =>
      choice(
        "||",
        "&&",
        "==",
        "!=",
        "<",
        "<=",
        ">",
        ">=",
        "+",
        "-",
        "*",
        "/",
        "!",
      ),
  },
});

function binary($, precedence, operator) {
  return prec.left(
    precedence,
    seq(
      field("left", $.expression),
      field("operator", alias(operator, $.operator)),
      field("right", $.expression),
    ),
  );
}

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}
