# Nexa editor support

This directory contains local language support for `.nexa` and `.nidl` files:

- `tree-sitter-nexa/` is the shared Tree-sitter grammar.
- `vscode/` is a code-free VS Code Language Basics extension.
- `zed/` is a two-language Zed extension template.
- `language-syntax.json` is the shared lexical vocabulary.
- `scripts/` generates, validates, and packages the extensions.

Build everything from the repository root:

```sh
pnpm --dir editors install --frozen-lockfile
pnpm --dir editors package
```

Artifacts are written below `target/nexa-editor-support/`. See
[`docs/EDITOR_SUPPORT.md`](../docs/EDITOR_SUPPORT.md) for installation,
maintenance, supported syntax, and limitations.
