(doc_comment) @comment.documentation
(line_comment) @comment
(block_comment) @comment

(contract_keyword) @keyword
(handle_keyword) @keyword
(nidl_struct_keyword) @keyword
(nidl_enum_keyword) @keyword
(host_keyword) @keyword
(nexa_keyword) @keyword
(async_keyword) @keyword
(nidl_function_keyword) @keyword

(nidl_attribute
  "@" @punctuation.delimiter
  name: (nidl_attribute_name) @attribute)

(policy_value) @constant
(nidl_builtin_type) @type.builtin
(nidl_type_identifier) @type

(contract_header
  name: (nidl_type_identifier) @type)

(handle_declaration
  name: (nidl_type_identifier) @type)

(nidl_struct_declaration
  name: (nidl_type_identifier) @type)

(nidl_enum_declaration
  name: (nidl_type_identifier) @type)

(host_function_declaration
  name: (nidl_identifier) @function)

(nexa_function_declaration
  name: (nidl_identifier) @function)

(nidl_parameter
  name: (nidl_identifier) @variable.parameter)

(nidl_field_declaration
  name: (nidl_identifier) @property)

(nidl_enum_variant
  name: (_) @constructor)

(integer_literal) @number
(string_literal) @string
(string_content) @string
(escape_sequence) @string.escape
(return_arrow_operator) @operator

["=" ":" "<" ">"] @operator

["(" ")" "{" "}" "<" ">"] @punctuation.bracket

["," ";" "@"] @punctuation.delimiter
