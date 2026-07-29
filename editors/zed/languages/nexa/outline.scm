(module_declaration
  keyword: (_) @context
  name: (_) @name) @item

(struct_declaration
  keyword: (_) @context
  name: (_) @name
  body: (_)) @item

(class_declaration
  keyword: (_) @context
  name: (_) @name
  body: (_)) @item

(enum_declaration
  keyword: (_) @context
  name: (_) @name
  body: (_)) @item

(enum_variant
  name: (_) @name) @item

(function_declaration
  keyword: (_) @context
  name: (_) @name
  parameters: (_) @context
  body: (_)) @item
