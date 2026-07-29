# Nexa Language Support

Provides syntax highlighting, bracket matching, automatic closing pairs, and
language identification for:

- Nexa source files (`.nexa`)
- Nexa interface definition files (`.nidl`)

The extension follows the syntax accepted by the Nexa compiler and IDL parser.
Neither language currently defines line or block comments, so comment commands
are intentionally not registered.

This package does not provide a language server, diagnostics, completion,
formatting, or debugging.
