(contract_header
  keyword: (_) @context
  name: (_) @name) @item

(handle_declaration
  keyword: (_) @context
  name: (_) @name) @item

(nidl_struct_declaration
  keyword: (_) @context
  name: (_) @name) @item

(nidl_enum_declaration
  keyword: (_) @context
  name: (_) @name) @item

(nidl_enum_variant
  name: (_) @name) @item

(host_block
  keyword: (_) @name) @item

(nexa_block
  keyword: (_) @name) @item

(host_function_declaration
  keyword: (_) @context
  name: (_) @name
  parameters: (_) @context) @item

(nexa_function_declaration
  keyword: (_) @context
  name: (_) @name
  parameters: (_) @context) @item
