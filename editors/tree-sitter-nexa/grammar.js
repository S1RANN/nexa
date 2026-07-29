const syntax = require("../language-syntax.json");

const nexaKeywords = Object.fromEntries(
  [
    ...syntax.nexa.declarationKeywords,
    ...syntax.nexa.effectKeywords,
    ...syntax.nexa.statementKeywords,
    ...syntax.nexa.attributeKeywords,
    ...syntax.nexa.literalKeywords,
  ].map((keyword) => [keyword, keyword]),
);
const nidlKeywords = Object.fromEntries(
  [
    ...syntax.nidl.declarationKeywords,
    ...syntax.nidl.modeKeywords,
    ...syntax.nidl.policyKeywords,
  ].map((keyword) => [keyword, keyword]),
);
const dottedMigrationIntrinsics = syntax.nexa.migrationIntrinsics.filter(
  (intrinsic) => intrinsic.includes("."),
);
const bareMigrationIntrinsics = syntax.nexa.migrationIntrinsics.filter(
  (intrinsic) => !intrinsic.includes("."),
);

const PREC = {
  EQUALITY: 1,
  ADDITIVE: 2,
  MULTIPLICATIVE: 3,
  AWAIT: 4,
  WITH: 5,
  TRY: 6,
  CALL: 7,
};

module.exports = grammar({
  name: "nexa",

  extras: ($) => [/\s/],

  word: ($) => $.lower_identifier,

  supertypes: ($) => [
    $.declaration,
    $.statement,
    $.expression,
    $.type,
    $.nidl_member,
    $.nidl_type,
  ],

  rules: {
    source_file: ($) => choice($.nexa_module, $.nidl_document),

    nexa_module: ($) =>
      choice(
        seq(
          $.module_declaration,
          repeat($.import_declaration),
          repeat($.declaration),
        ),
        seq(repeat1($.import_declaration), repeat($.declaration)),
        repeat1($.declaration),
      ),

    module_declaration: ($) =>
      seq(
        field("keyword", $.module_keyword),
        field("name", choice($.identifier, $.qualified_identifier)),
        ";",
      ),

    import_declaration: ($) =>
      seq(
        field("keyword", $.import_keyword),
        field("name", choice($.identifier, $.qualified_identifier)),
        ";",
      ),

    declaration: ($) =>
      choice(
        $.struct_declaration,
        $.enum_declaration,
        $.class_declaration,
        $.function_declaration,
      ),

    struct_declaration: ($) =>
      seq(
        optional($.stateful_attribute),
        field("keyword", $.struct_keyword),
        field("name", $.type_identifier),
        field("body", $.field_declaration_block),
      ),

    class_declaration: ($) =>
      seq(
        optional($.stateful_attribute),
        field("keyword", $.class_keyword),
        field("name", $.type_identifier),
        field("body", $.field_declaration_block),
      ),

    enum_declaration: ($) =>
      seq(
        optional($.stateful_attribute),
        field("keyword", $.enum_keyword),
        field("name", $.type_identifier),
        field("body", $.enum_variant_block),
      ),

    stateful_attribute: ($) =>
      seq(
        "@",
        field("name", $.stateful_keyword),
        optional(seq("(", field("version", $.integer_literal), ")")),
      ),

    activation_attribute: ($) =>
      seq("@", field("name", $.activation_keyword)),

    field_declaration_block: ($) =>
      seq("{", repeat($.field_declaration), "}"),

    field_declaration: ($) =>
      seq(
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
        optional($.activation_attribute),
        optional(field("effect", $.effect_keyword)),
        field("keyword", $.function_keyword),
        field("name", $.identifier),
        field("parameters", $.parameter_list),
        field("arrow", $.return_arrow_operator),
        field("return_type", $.type),
        field("body", $.block),
      ),

    parameter_list: ($) =>
      seq("(", optional(commaSep1($.parameter)), ")"),

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
            seq($.else_keyword, field("alternative", $.block)),
          ),
        ),
      ),

    for_statement: ($) =>
      seq(
        $.for_keyword,
        field("variable", $.identifier),
        $.in_keyword,
        field("start", $.integer_literal),
        "..",
        field("end", $.integer_literal),
        field("body", $.block),
      ),

    while_statement: ($) =>
      seq(
        $.while_keyword,
        field("condition", $.expression),
        field("body", $.block),
      ),

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
      prec(
        PREC.AWAIT,
        seq($.await_keyword, field("value", $.expression)),
      ),

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
                field(
                  "namespace",
                  alias(namespace, $.migration_namespace),
                ),
                ".",
                field(
                  "function",
                  alias(functionName, $.migration_function),
                ),
              );
            }),
          ),
          optional(field("type_arguments", $.type_argument_list)),
          field("arguments", $.argument_list),
        ),
      ),

    collection_type: ($) =>
      choice(
        ...syntax.nexa.builtinTypes.filter((type) =>
          ["Array", "Map"].includes(type),
        ),
      ),

    migration_namespace: ($) =>
      choice(
        ...new Set(
          dottedMigrationIntrinsics.map((intrinsic) => intrinsic.split(".")[0]),
        ),
      ),

    migration_function: ($) =>
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

    migration_intrinsic_name: ($) => choice(...bareMigrationIntrinsics),

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

    constructor_name: ($) => choice(...syntax.nexa.constructors),

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
        prec.left(
          PREC.EQUALITY,
          seq(
            field("left", $.expression),
            field("operator", alias("==", $.operator)),
            field("right", $.expression),
          ),
        ),
        prec.left(
          PREC.ADDITIVE,
          seq(
            field("left", $.expression),
            field("operator", alias(choice("+", "-"), $.operator)),
            field("right", $.expression),
          ),
        ),
        prec.left(
          PREC.MULTIPLICATIVE,
          seq(
            field("left", $.expression),
            field("operator", alias(choice("*", "/"), $.operator)),
            field("right", $.expression),
          ),
        ),
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

    argument_list: ($) =>
      seq("(", optional(commaSep1($.expression)), ")"),

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
      choice($.builtin_type, $.generic_type, $.type_identifier),

    generic_type: ($) =>
      seq(
        field("name", choice($.builtin_type, $.type_identifier)),
        field("arguments", $.type_argument_list),
      ),

    type_argument_list: ($) => seq("<", commaSep1($.type), ">"),

    qualified_identifier: ($) =>
      prec.left(
        seq($.identifier, repeat1(seq(".", $.identifier))),
      ),

    type_identifier: ($) => $.identifier,

    identifier: ($) => choice($.lower_identifier, $.upper_identifier),

    lower_identifier: ($) => /[a-z_][A-Za-z0-9_]*/,

    upper_identifier: ($) => /[A-Z][A-Za-z0-9_]*/,

    builtin_type: ($) => choice(...syntax.nexa.builtinTypes),

    boolean_literal: ($) => choice(...syntax.nexa.literalKeywords),

    integer_literal: ($) => /[0-9]+/,

    float_literal: ($) => /[0-9]+\.[0-9]+/,

    string_literal: ($) =>
      token(seq('"', repeat(choice(/[^"\\]/, /\\[nrt\\"]/)), '"')),

    rune_literal: ($) =>
      token(seq("'", choice(/[^'\\]/, /\\[nrt\\']/), "'")),

    module_keyword: ($) => nexaKeywords.module,
    import_keyword: ($) => nexaKeywords.import,
    function_keyword: ($) => nexaKeywords.fn,
    struct_keyword: ($) => nexaKeywords.struct,
    enum_keyword: ($) => nexaKeywords.enum,
    class_keyword: ($) => nexaKeywords.class,
    effect_keyword: ($) => choice(...syntax.nexa.effectKeywords),
    stateful_keyword: ($) => nexaKeywords.stateful,
    activation_keyword: ($) => nexaKeywords.activation,
    return_keyword: ($) => nexaKeywords.return,
    let_keyword: ($) => nexaKeywords.let,
    var_keyword: ($) => nexaKeywords.var,
    if_keyword: ($) => nexaKeywords.if,
    else_keyword: ($) => nexaKeywords.else,
    while_keyword: ($) => nexaKeywords.while,
    match_keyword: ($) => nexaKeywords.match,
    with_keyword: ($) => nexaKeywords.with,
    new_keyword: ($) => nexaKeywords.new,
    await_keyword: ($) => nexaKeywords.await,
    yield_keyword: ($) => nexaKeywords.yield,
    defer_keyword: ($) => nexaKeywords.defer,
    for_keyword: ($) => nexaKeywords.for,
    in_keyword: ($) => nexaKeywords.in,

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

    nidl_type_argument_list: ($) =>
      seq("<", commaSep1($.nidl_type), ">"),

    nidl_type_identifier: ($) => $.nidl_identifier,

    nidl_identifier: ($) => /[^{}(),:;<>\s-]+/,

    interface_keyword: ($) => nidlKeywords.interface,
    opaque_keyword: ($) => nidlKeywords.opaque,
    nidl_struct_keyword: ($) => nidlKeywords.struct,
    nidl_enum_keyword: ($) => nidlKeywords.enum,
    nidl_function_keyword: ($) => nidlKeywords.fn,
    export_keyword: ($) => nidlKeywords.export,
    host_mode_keyword: ($) =>
      choice(
        ...syntax.nidl.modeKeywords.filter((keyword) => keyword !== "fuel"),
      ),
    fuel_keyword: ($) => nidlKeywords.fuel,
    cancel_policy_keyword: ($) =>
      choice(
        ...syntax.nidl.policyKeywords.filter((keyword) => keyword !== "trap"),
      ),
    abandon_policy_keyword: ($) =>
      choice(
        ...syntax.nidl.policyKeywords.filter(
          (keyword) => keyword !== "cancel_task",
        ),
      ),
    nidl_builtin_type: ($) =>
      choice(
        ...syntax.nidl.builtinTypes.filter((type) => type !== "void"),
      ),
    void_type: ($) =>
      syntax.nidl.builtinTypes.find((type) => type === "void"),

    return_arrow_operator: ($) => "->",

    match_arrow_operator: ($) => "=>",

    operator: ($) => choice("==", "+", "-", "*", "/"),
  },
});

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}
