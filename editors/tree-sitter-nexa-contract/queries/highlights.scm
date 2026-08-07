(doc_comment) @comment.documentation
(line_comment) @comment
(block_comment) @comment

(contract_keyword) @keyword
(handle_keyword) @keyword
(contract_struct_keyword) @keyword
(contract_enum_keyword) @keyword
(host_keyword) @keyword
(nexa_keyword) @keyword
(async_keyword) @keyword
(contract_function_keyword) @keyword

(contract_attribute
  "@" @punctuation.delimiter
  name: (contract_attribute_name) @attribute)

(policy_value) @constant
(contract_builtin_type) @type.builtin
(contract_type_identifier) @type

(contract_header
  name: (contract_type_identifier) @type)

(handle_declaration
  name: (contract_type_identifier) @type)

(contract_struct_declaration
  name: (contract_type_identifier) @type)

(contract_enum_declaration
  name: (contract_type_identifier) @type)

(host_function_declaration
  name: (contract_identifier) @function)

(nexa_function_declaration
  name: (contract_identifier) @function)

(contract_parameter
  name: (contract_identifier) @variable.parameter)

(contract_field_declaration
  name: (contract_identifier) @property)

(contract_enum_variant
  name: (_) @constructor)

(integer_literal) @number
(string_literal) @string
(string_content) @string
(escape_sequence) @string.escape
(return_arrow_operator) @operator

["=" ":" "<" ">"] @operator

["(" ")" "{" "}" "<" ">"] @punctuation.bracket

["," ";" "@"] @punctuation.delimiter
