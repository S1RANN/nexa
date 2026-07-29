(module_keyword) @keyword
(import_keyword) @keyword
(function_keyword) @keyword
(struct_keyword) @keyword
(enum_keyword) @keyword
(class_keyword) @keyword
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
(stateful_keyword) @attribute
(activation_keyword) @attribute

(interface_keyword) @keyword
(opaque_keyword) @keyword
(nidl_struct_keyword) @keyword
(nidl_enum_keyword) @keyword
(nidl_function_keyword) @keyword
(export_keyword) @keyword
(host_mode_keyword) @keyword
(fuel_keyword) @keyword
(cancel_policy_keyword) @constant
(abandon_policy_keyword) @constant

(builtin_type) @type.builtin
(nidl_builtin_type) @type.builtin
(void_type) @type.builtin
(type_identifier) @type
(nidl_type_identifier) @type

(struct_literal
  type: (upper_identifier) @type)

(new_expression
  type: (type_identifier) @type)

(function_declaration
  name: (identifier) @function)

(host_function_declaration
  name: (nidl_identifier) @function)

(export_declaration
  name: (_) @function)

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

(nidl_parameter
  name: (nidl_identifier) @variable.parameter)

(field_declaration
  name: (identifier) @property)

(nidl_field_declaration
  name: (nidl_identifier) @property)

(field_initializer
  name: (identifier) @property)

(member_expression
  property: (identifier) @property)

(enum_variant
  name: (_) @constructor)

(nidl_enum_variant
  name: (_) @constructor)

(match_arm
  variant: (_) @constructor)

(boolean_literal) @boolean
(integer_literal) @number
(float_literal) @number
(string_literal) @string
(rune_literal) @string.special
(operator) @operator

[
  "->"
  "=>"
  "="
  "?"
  ".."
  ":"
] @operator

[
  "("
  ")"
  "{"
  "}"
  "<"
  ">"
] @punctuation.bracket

[
  ","
  ";"
  "."
  "@"
] @punctuation.delimiter
