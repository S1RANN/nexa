# Nexa Language Support

Provides syntax highlighting, bracket matching, automatic closing pairs,
language identification, and real-time Problems diagnostics for:

- Nexa source files (`.nexa`)
- Nexa Host Contract files (`*.contract.nexa`, language id `nexa-contract`)

The extension follows the syntax accepted by the Nexa compiler and Contract
parser. Nexa v2 highlighting includes `use` and `package::`/`host::` paths,
mutable bindings and fields, attributes, `async fn`, and postfix `.await`.
Contract (v3) highlighting covers the flat `contract <Name>;` header (with
`@stable(...)` attributes and doc comments), `handle`, `host`/`nexa` blocks,
attributes, generic types, and `//`, `///`, and non-nested `/* ... */`
comments. Comment commands are registered for both languages.

The extension starts `nexa lsp` from `PATH`. Set `nexa.server.path` when the
CLI is installed elsewhere. The language server provides diagnostics and a
Contract document outline (Contract/Struct/Enum/Handle/Host Function/Nexa
Function); completion, navigation, formatting, refactoring, semantic tokens,
and full debugging remain outside M4.
