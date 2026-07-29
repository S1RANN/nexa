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

(nidl_builtin_type) @type.builtin
(void_type) @type.builtin
(nidl_type_identifier) @type

(nidl_document
  name: (nidl_type_identifier) @type)

(opaque_declaration
  name: (nidl_type_identifier) @type)

(nidl_struct_declaration
  name: (nidl_type_identifier) @type)

(nidl_enum_declaration
  name: (nidl_type_identifier) @type)

(host_function_declaration
  name: (nidl_identifier) @function)

(export_declaration
  name: (_) @function)

(nidl_parameter
  name: (nidl_identifier) @variable.parameter)

(nidl_field_declaration
  name: (nidl_identifier) @property)

(nidl_enum_variant
  name: (_) @constructor)

(integer_literal) @number

[
  "->"
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
] @punctuation.delimiter
