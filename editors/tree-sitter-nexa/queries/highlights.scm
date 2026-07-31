(doc_comment) @comment.documentation
(line_comment) @comment
(block_comment) @comment

(module_keyword) @keyword
(import_keyword) @keyword
(as_keyword) @keyword
(function_keyword) @keyword
(struct_keyword) @keyword
(enum_keyword) @keyword
(class_keyword) @keyword
(const_keyword) @keyword
(pub_keyword) @keyword
(package_keyword) @keyword
(effect_keyword) @keyword
(return_keyword) @keyword
(let_keyword) @keyword
(var_keyword) @keyword
(if_keyword) @keyword
(else_keyword) @keyword
(while_keyword) @keyword
(match_keyword) @keyword
(with_keyword) @keyword
(new_keyword) @keyword
(await_keyword) @keyword
(yield_keyword) @keyword
(defer_keyword) @keyword
(for_keyword) @keyword
(in_keyword) @keyword
(break_keyword) @keyword
(continue_keyword) @keyword

(stateful_attribute
  "@" @punctuation.delimiter
  name: (stateful_keyword) @attribute)

(activation_attribute
  "@" @punctuation.delimiter
  name: (activation_keyword) @attribute)

(stable_attribute
  "@" @punctuation.delimiter
  name: (stable_keyword) @attribute)

(test_attribute
  "@" @punctuation.delimiter
  name: (test_keyword) @attribute)

(module_declaration
  name: (module_path) @module)

(import_declaration
  name: (_) @module)

(import_declaration
  alias: (lower_identifier) @module)

(builtin_type) @type.builtin
(type_identifier) @type
(qualified_type_identifier) @type

(struct_literal
  type: (upper_identifier) @type)

(new_expression
  type: (type_identifier) @type)

(struct_declaration
  name: (type_identifier) @type)

(class_declaration
  name: (type_identifier) @type)

(enum_declaration
  name: (type_identifier) @type)

(function_declaration
  name: (identifier) @function)

(const_declaration
  name: (identifier) @constant)

(call_expression
  function: (_) @function.call)

(collection_new_expression
  collection: (collection_type) @type.builtin
  constructor: (new_keyword) @function.call)

(migration_member_call
  namespace: (migration_namespace) @function.special
  function: (migration_function) @function.special)

(migration_intrinsic_name) @function.special
(constructor_name) @constructor

(parameter
  name: (identifier) @variable.parameter)

(binding_statement
  name: (identifier) @variable)

(for_statement
  variable: (identifier) @variable)

(field_declaration
  name: (identifier) @property)

(field_initializer
  name: (identifier) @property)

(member_expression
  property: (identifier) @property)

(enum_variant
  name: (_) @constructor)

(match_arm
  variant: (_) @constructor)

(boolean_literal) @boolean
(integer_literal) @number
(float_literal) @number
(string_literal) @string
(string_content) @string
(escape_sequence) @string.escape
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)
(rune_literal) @string.special
(operator) @operator
(return_arrow_operator) @operator
(match_arrow_operator) @operator

["=" "?" ".." ":"] @operator

["(" ")" "{" "}" "<" ">"] @punctuation.bracket

["," ";" "." "@"] @punctuation.delimiter
