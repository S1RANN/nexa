# Nexa Language Support

Provides syntax highlighting, bracket matching, automatic closing pairs,
language identification, and real-time Problems diagnostics for:

- Nexa source files (`.nexa`)
- Nexa interface definition files (`.nidl`)

The extension follows the syntax accepted by the Nexa compiler and IDL parser.
Neither language currently defines line or block comments, so comment commands
are intentionally not registered.

The extension starts `nexa lsp` from `PATH`. Set `nexa.server.path` when the
CLI is installed elsewhere. The language server intentionally provides only
diagnostics; completion, navigation, formatting, refactoring, semantic tokens,
and debugging remain outside M3.
