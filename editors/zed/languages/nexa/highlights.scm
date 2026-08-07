(doc_comment) @comment.documentation
(line_comment) @comment
(block_comment) @comment

(use_keyword) @keyword
(as_keyword) @keyword
(function_keyword) @keyword
(struct_keyword) @keyword
(enum_keyword) @keyword
(class_keyword) @keyword
(const_keyword) @keyword
(pub_keyword) @keyword
(package_keyword) @keyword
(async_keyword) @keyword
(return_keyword) @keyword
(let_keyword) @keyword
(if_keyword) @keyword
(else_keyword) @keyword
(while_keyword) @keyword
(match_keyword) @keyword
(new_keyword) @keyword
(await_keyword) @keyword
(yield_keyword) @keyword
(defer_keyword) @keyword
(for_keyword) @keyword
(in_keyword) @keyword
(break_keyword) @keyword
(continue_keyword) @keyword

(attribute
  "@" @punctuation.delimiter
  name: (attribute_name) @attribute)

(use_declaration
  path: (namespace_path) @module)

(use_declaration
  alias: (lower_identifier) @module)

(namespace_path
  root: (identifier) @module)

(builtin_type) @type.builtin
(type_path) @type

(aggregate_literal
  type: (type_path) @type)

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

(postfix_expression
  operand: (_) @function.call
  operation: (call_suffix))

(parameter
  name: (identifier) @variable.parameter)

(binding_statement
  name: (identifier) @variable)

(for_statement
  variable: (identifier) @variable)

(pair_binding
  key: (identifier) @variable
  value: (identifier) @variable)

(field_declaration
  name: (identifier) @property)

(field_initializer
  name: (identifier) @property)

(field_suffix
  property: (identifier) @property)

(enum_variant
  name: (_) @constructor)

(match_pattern
  (path_expression) @constructor)

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
(try_suffix) @operator

["=" ".." "::" ":"] @operator

["(" ")" "[" "]" "{" "}" "<" ">"] @punctuation.bracket

["," ";" "." "@"] @punctuation.delimiter
