const syntax = require("../language-syntax.json");

const nexaKeywords = Object.fromEntries(
  [
    ...syntax.nexa.declarationKeywords,
    ...syntax.nexa.visibilityKeywords,
    ...syntax.nexa.effectKeywords,
    ...syntax.nexa.statementKeywords,
    ...syntax.nexa.attributeKeywords,
    ...syntax.nexa.literalKeywords,
  ].map((keyword) => [keyword, keyword]),
);
const dottedMigrationIntrinsics = syntax.nexa.migrationIntrinsics.filter(
  (intrinsic) => intrinsic.includes("."),
);
const bareMigrationIntrinsics = syntax.nexa.migrationIntrinsics.filter(
  (intrinsic) => !intrinsic.includes("."),
);

const PREC = {
  LOGICAL_OR: 1,
  LOGICAL_AND: 2,
  EQUALITY: 3,
  COMPARISON: 4,
  ADDITIVE: 5,
  MULTIPLICATIVE: 6,
  UNARY: 7,
  AWAIT: 8,
  WITH: 9,
  TRY: 10,
  CALL: 11,
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

  supertypes: ($) => [$.declaration, $.statement, $.expression, $.type],

  rules: {
    source_file: ($) =>
      seq(
        optional($.module_declaration),
        repeat($.import_declaration),
        repeat($.declaration),
      ),

    doc_comment: (_) => token(prec(2, /\/\/\/[^\n\r]*/)),
    line_comment: (_) => token(prec(1, /\/\/[^\n\r]*/)),
    block_comment: (_) => token(seq("/*", /[^*]*\*+([^/*][^*]*\*+)*/, "/")),

    module_declaration: ($) =>
      seq(
        field("keyword", $.module_keyword),
        field("name", $.module_path),
        ";",
      ),

    import_declaration: ($) =>
      seq(
        field("keyword", $.import_keyword),
        field("name", choice($.module_path, $.host_module)),
        optional(
          seq(
            field("alias_keyword", $.as_keyword),
            field("alias", $.lower_identifier),
          ),
        ),
        ";",
      ),

    module_path: ($) =>
      prec.left(seq($.lower_identifier, repeat(seq(".", $.lower_identifier)))),

    host_module: (_) => "host",

    declaration: ($) =>
      choice(
        $.struct_declaration,
        $.enum_declaration,
        $.class_declaration,
        $.function_declaration,
        $.const_declaration,
      ),

    attribute: ($) =>
      choice(
        $.stateful_attribute,
        $.activation_attribute,
        $.stable_attribute,
        $.test_attribute,
      ),

    stateful_attribute: ($) =>
      seq(
        "@",
        field("name", $.stateful_keyword),
        optional(seq("(", field("version", $.integer_literal), ")")),
      ),

    activation_attribute: ($) =>
      seq("@", field("name", $.activation_keyword)),

    stable_attribute: ($) =>
      seq(
        "@",
        field("name", $.stable_keyword),
        "(",
        field("identity", $.string_literal),
        ")",
      ),

    test_attribute: ($) => seq("@", field("name", $.test_keyword)),

    visibility: ($) =>
      choice(
        $.pub_keyword,
        seq($.pub_keyword, "(", $.package_keyword, ")"),
      ),

    struct_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        field("keyword", $.struct_keyword),
        field("name", $.type_identifier),
        field("body", $.field_declaration_block),
      ),

    class_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        field("keyword", $.class_keyword),
        field("name", $.type_identifier),
        field("body", $.field_declaration_block),
      ),

    enum_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        field("keyword", $.enum_keyword),
        field("name", $.type_identifier),
        field("body", $.enum_variant_block),
      ),

    const_declaration: ($) =>
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

    field_declaration_block: ($) =>
      seq("{", repeat($.field_declaration), "}"),

    field_declaration: ($) =>
      seq(
        repeat($.stable_attribute),
        field("name", $.identifier),
        ":",
        field("type", $.type),
        ";",
      ),

    enum_variant_block: ($) =>
      seq("{", repeat(seq($.enum_variant, optional(","))), "}"),

    enum_variant: ($) =>
      seq(
        field("name", $.type_identifier),
        optional(seq("(", field("payload", $.type), ")")),
      ),

    function_declaration: ($) =>
      seq(
        repeat($.attribute),
        optional(field("visibility", $.visibility)),
        optional(field("effect", $.effect_keyword)),
        field("keyword", $.function_keyword),
        field("name", $.identifier),
        field("parameters", $.parameter_list),
        field("arrow", $.return_arrow_operator),
        field("return_type", $.type),
        field("body", $.block),
      ),

    parameter_list: ($) => seq("(", optional(commaSep1($.parameter)), ")"),

    parameter: ($) =>
      seq(field("name", $.identifier), ":", field("type", $.type)),

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
      seq($.return_keyword, field("value", $.expression), ";"),

    binding_statement: ($) =>
      seq(
        field("keyword", choice($.let_keyword, $.var_keyword)),
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
      seq(
        $.for_keyword,
        field("variable", $.identifier),
        $.in_keyword,
        field("start", $.expression),
        "..",
        field("end", $.expression),
        field("body", $.block),
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
      seq($.defer_keyword, field("value", $.expression), ";"),

    assignment_statement: ($) =>
      seq(
        field("target", $.member_expression),
        "=",
        field("value", $.expression),
        ";",
      ),

    expression_statement: ($) => seq($.expression, ";"),

    expression: ($) =>
      choice(
        $.await_expression,
        $.match_expression,
        $.collection_new_expression,
        $.migration_member_call,
        $.migration_intrinsic_call,
        $.constructor_expression,
        $.new_expression,
        $.struct_literal,
        $.with_expression,
        $.try_expression,
        $.binary_expression,
        $.unary_expression,
        $.call_expression,
        $.generic_name,
        $.member_expression,
        $.parenthesized_expression,
        $.boolean_literal,
        $.float_literal,
        $.integer_literal,
        $.rune_literal,
        $.string_literal,
        $.identifier,
      ),

    await_expression: ($) =>
      prec(PREC.AWAIT, seq($.await_keyword, field("value", $.expression))),

    match_expression: ($) =>
      prec.right(
        seq(
          $.match_keyword,
          field("value", $.expression),
          "{",
          repeat(seq($.match_arm, optional(","))),
          "}",
        ),
      ),

    match_arm: ($) =>
      seq(
        field("variant", $.type_identifier),
        optional(seq("(", field("binding", $.identifier), ")")),
        field("arrow", $.match_arrow_operator),
        field("value", $.expression),
      ),

    new_expression: ($) =>
      prec(
        PREC.CALL,
        seq(
          $.new_keyword,
          field("type", $.type_identifier),
          field("fields", $.field_initializer_block),
        ),
      ),

    collection_new_expression: ($) =>
      prec(
        PREC.CALL,
        seq(
          field("collection", $.collection_type),
          ".",
          field("constructor", $.new_keyword),
          field("type_arguments", $.type_argument_list),
          field("arguments", $.argument_list),
        ),
      ),

    migration_member_call: ($) =>
      prec(
        PREC.CALL,
        seq(
          choice(
            ...dottedMigrationIntrinsics.map((intrinsic) => {
              const [namespace, functionName] = intrinsic.split(".");
              return seq(
                field("namespace", alias(namespace, $.migration_namespace)),
                ".",
                field("function", alias(functionName, $.migration_function)),
              );
            }),
          ),
          optional(field("type_arguments", $.type_argument_list)),
          field("arguments", $.argument_list),
        ),
      ),

    collection_type: (_) =>
      choice(
        ...syntax.nexa.builtinTypes.filter((type) =>
          ["Array", "Map"].includes(type),
        ),
      ),

    migration_namespace: (_) =>
      choice(
        ...new Set(
          dottedMigrationIntrinsics.map((intrinsic) => intrinsic.split(".")[0]),
        ),
      ),

    migration_function: (_) =>
      choice(
        ...new Set(
          dottedMigrationIntrinsics.map((intrinsic) => intrinsic.split(".")[1]),
        ),
      ),

    migration_intrinsic_call: ($) =>
      prec(
        PREC.CALL,
        seq(
          field("function", $.migration_intrinsic_name),
          optional(field("type_arguments", $.type_argument_list)),
          field("arguments", $.argument_list),
        ),
      ),

    migration_intrinsic_name: (_) => choice(...bareMigrationIntrinsics),

    constructor_expression: ($) =>
      prec(
        PREC.CALL,
        choice(
          field("constructor", alias("None", $.constructor_name)),
          seq(
            field(
              "constructor",
              alias(
                choice(
                  ...syntax.nexa.constructors.filter(
                    (constructor) => constructor !== "None",
                  ),
                ),
                $.constructor_name,
              ),
            ),
            "(",
            field("payload", $.expression),
            ")",
          ),
        ),
      ),

    constructor_name: (_) => choice(...syntax.nexa.constructors),

    struct_literal: ($) =>
      prec(
        PREC.CALL,
        seq(
          field("type", $.upper_identifier),
          field("fields", $.field_initializer_block),
        ),
      ),

    field_initializer_block: ($) =>
      seq("{", optional(commaSep1($.field_initializer)), optional(","), "}"),

    field_initializer: ($) =>
      seq(
        field("name", $.identifier),
        ":",
        field("value", $.expression),
      ),

    with_expression: ($) =>
      prec.left(
        PREC.WITH,
        seq(
          field("value", $.expression),
          $.with_keyword,
          field("updates", $.field_initializer_block),
        ),
      ),

    try_expression: ($) =>
      prec.left(PREC.TRY, seq(field("value", $.expression), "?")),

    binary_expression: ($) =>
      choice(
        binary($, PREC.LOGICAL_OR, "||"),
        binary($, PREC.LOGICAL_AND, "&&"),
        binary($, PREC.EQUALITY, choice("==", "!=")),
        binary($, PREC.COMPARISON, choice("<", "<=", ">", ">=")),
        binary($, PREC.ADDITIVE, choice("+", "-")),
        binary($, PREC.MULTIPLICATIVE, choice("*", "/")),
      ),

    unary_expression: ($) =>
      prec(
        PREC.UNARY,
        seq(field("operator", alias(choice("!", "-"), $.operator)), $.expression),
      ),

    call_expression: ($) =>
      prec(
        PREC.CALL,
        seq(
          field(
            "function",
            choice(
              $.identifier,
              $.qualified_identifier,
              $.member_expression,
              $.generic_name,
            ),
          ),
          field("arguments", $.argument_list),
        ),
      ),

    argument_list: ($) => seq("(", optional(commaSep1($.expression)), ")"),

    generic_name: ($) =>
      prec(
        PREC.CALL,
        seq(
          field(
            "name",
            choice($.identifier, $.qualified_identifier, $.member_expression),
          ),
          field("type_arguments", $.type_argument_list),
        ),
      ),

    member_expression: ($) =>
      prec.left(
        PREC.CALL,
        seq(
          field(
            "object",
            choice($.identifier, $.qualified_identifier, $.call_expression),
          ),
          ".",
          field("property", $.identifier),
        ),
      ),

    parenthesized_expression: ($) => seq("(", $.expression, ")"),

    type: ($) =>
      choice(
        $.builtin_type,
        $.generic_type,
        $.type_identifier,
        $.qualified_type_identifier,
      ),

    generic_type: ($) =>
      seq(
        field(
          "name",
          choice(
            $.builtin_type,
            $.type_identifier,
            $.qualified_type_identifier,
          ),
        ),
        field("arguments", $.type_argument_list),
      ),

    type_argument_list: ($) => seq("<", commaSep1($.type), ">"),

    qualified_type_identifier: ($) =>
      prec.left(
        seq($.lower_identifier, repeat1(seq(".", $.identifier))),
      ),

    qualified_identifier: ($) =>
      prec.left(seq($.identifier, repeat1(seq(".", $.identifier)))),

    type_identifier: ($) => $.identifier,
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
    escape_sequence: (_) =>
      token.immediate(/\\(?:[nrt\\"']|\$\{)/),
    interpolation: ($) =>
      seq(
        token.immediate("${"),
        field("expression", $.expression),
        "}",
      ),

    rune_literal: (_) =>
      token(seq("'", choice(/[^'\\]/, /\\[nrt\\']/), "'")),

    module_keyword: (_) => nexaKeywords.module,
    import_keyword: (_) => nexaKeywords.import,
    as_keyword: (_) => nexaKeywords.as,
    function_keyword: (_) => nexaKeywords.fn,
    struct_keyword: (_) => nexaKeywords.struct,
    enum_keyword: (_) => nexaKeywords.enum,
    class_keyword: (_) => nexaKeywords.class,
    const_keyword: (_) => nexaKeywords.const,
    pub_keyword: (_) => nexaKeywords.pub,
    package_keyword: (_) => nexaKeywords.package,
    effect_keyword: (_) => choice(...syntax.nexa.effectKeywords),
    stateful_keyword: (_) => nexaKeywords.stateful,
    activation_keyword: (_) => nexaKeywords.activation,
    stable_keyword: (_) => nexaKeywords.stable,
    test_keyword: (_) => nexaKeywords.test,
    return_keyword: (_) => nexaKeywords.return,
    let_keyword: (_) => nexaKeywords.let,
    var_keyword: (_) => nexaKeywords.var,
    if_keyword: (_) => nexaKeywords.if,
    else_keyword: (_) => nexaKeywords.else,
    while_keyword: (_) => nexaKeywords.while,
    match_keyword: (_) => nexaKeywords.match,
    with_keyword: (_) => nexaKeywords.with,
    new_keyword: (_) => nexaKeywords.new,
    await_keyword: (_) => nexaKeywords.await,
    yield_keyword: (_) => nexaKeywords.yield,
    defer_keyword: (_) => nexaKeywords.defer,
    for_keyword: (_) => nexaKeywords.for,
    in_keyword: (_) => nexaKeywords.in,
    break_keyword: (_) => nexaKeywords.break,
    continue_keyword: (_) => nexaKeywords.continue,

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
