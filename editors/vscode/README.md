# Nexa Language Support

Provides syntax highlighting, bracket matching, automatic closing pairs,
language identification, and real-time Problems diagnostics for:

- Nexa source files (`.nexa`)
- Nexa contract definition files (`.nidl`)

The extension follows the syntax accepted by the Nexa compiler and IDL parser.
Nexa v2 highlighting includes `use` and `package::`/`host::` paths, mutable
bindings and fields, attributes, `async fn`, and postfix `.await`. NIDL v2
highlights `contract`, `handle`, `host`/`nexa` blocks, attributes, generic
types, and `//`, `///`, and non-nested `/* ... */` comments. Comment commands
are registered for both languages.

The extension starts `nexa lsp` from `PATH`. Set `nexa.server.path` when the
CLI is installed elsewhere. The language server intentionally provides only
diagnostics; completion, navigation, formatting, refactoring, semantic tokens,
and debugging remain outside M4.
